//! S5R 边界映射：领域错误 → 线缆错误码。
//!
//! 映射实现集中在本模块单点；`astrcode-core`/`astrcode-storage`/`astrcode-extension-sdk`
//! 不感知线缆概念，宿主路由层统一经 [`wire_payload`] 构造线缆错误，
//! 杜绝同一错误在不同 API 上映射不一致。

use std::fmt::Display;

use astrcode_core::{llm::LlmError, tool::SessionApiError};
use astrcode_extension_sdk::{
    host::internal::OutboundNetworkError,
    wire::{ErrorPayload, WireErrorCode},
};
use astrcode_storage::StorageError;

/// 领域错误 → 线缆错误码的映射协议。
///
/// 每个领域错误类型实现一次，宿主路由层统一经 [`wire_payload`] 构造线缆错误，
/// 杜绝同一错误在不同 API 上映射不一致。
pub(super) trait WireError: Display {
    fn wire_code(&self) -> WireErrorCode;

    fn is_retryable(&self) -> bool {
        false
    }
}

impl WireError for LlmError {
    fn wire_code(&self) -> WireErrorCode {
        match self {
            Self::InvalidApiKey { .. } => WireErrorCode::InvalidApiKey,
            Self::ModelNotFound { .. } => WireErrorCode::ModelNotFound,
            Self::InvalidParameter { .. } => WireErrorCode::InvalidInput,
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

impl WireError for SessionApiError {
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

impl WireError for StorageError {
    fn wire_code(&self) -> WireErrorCode {
        match self {
            Self::NotFound(_) => WireErrorCode::SessionNotFound,
            Self::AlreadyExists(_) => WireErrorCode::SessionAlreadyExists,
            Self::InvalidId(_) => WireErrorCode::InvalidInput,
            Self::Unsupported(_) => WireErrorCode::Unsupported,
            Self::Io(_) | Self::DurabilityUncertain { .. } => WireErrorCode::StorageIoError,
            Self::Serialization(_) | Self::InvalidEvent(_) | Self::CorruptLog(_) => {
                WireErrorCode::CorruptSessionData
            },
            Self::LockError(_) => WireErrorCode::StorageLockError,
        }
    }

    fn is_retryable(&self) -> bool {
        self.is_retryable()
    }
}

impl WireError for OutboundNetworkError {
    fn wire_code(&self) -> WireErrorCode {
        self.code
    }

    fn is_retryable(&self) -> bool {
        self.retryable
    }
}

pub(super) fn wire_payload<E: WireError>(error: E) -> ErrorPayload {
    ErrorPayload::new(error.wire_code(), error.to_string()).retryable(error.is_retryable())
}

#[cfg(test)]
mod tests {
    use astrcode_core::{llm::LlmError, tool::SessionApiError};

    use super::{WireError, WireErrorCode};

    #[test]
    fn domain_errors_map_consistently() {
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
