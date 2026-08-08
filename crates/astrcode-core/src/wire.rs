//! 扩展宿主与插件之间的线缆错误码与错误映射协议。
//!
//! 所有跨 wire 的错误码（宿主生产、worker/插件消费、测试断言）在此单点定义，
//! 序列化边界统一走 [`WireErrorCode::as_str`]，未知码在反序列化时透传保留。

use std::fmt::Display;

/// 线缆错误码的类型化表示。
///
/// wire 上始终是稳定字符串（[`WireErrorCode::as_str`]）；enum 只是让生产方
/// 无法拼错、让消费方可以穷举匹配。新增码必须同时更新 [`WireErrorCode::as_str`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireErrorCode {
    // ── 通用宿主码 ────────────────────────────────────────────────
    PermissionDenied,
    BackendUnavailable,
    ContextUnavailable,
    InvalidInput,
    Cancelled,
    Timeout,
    HostNotReady,
    PeerBusy,
    PeerClosed,
    Transport,
    IoError,
    UnknownCapability,
    StateTooLarge,
    SerializationFailed,
    InvalidResponse,
    Unsupported,
    NotSupported,
    HostRuntimeFailed,
    // ── network 域 ────────────────────────────────────────────────
    NetworkRequestFailed,
    ResponseTooLarge,
    // ── session 域 ────────────────────────────────────────────────
    SessionNotFound,
    SessionBusy,
    SessionAlreadyExists,
    MaxDepthExceeded,
    InternalError,
    InvalidRequest,
    DuplicateRequestId,
    // ── workspace 域 ──────────────────────────────────────────────
    ReadFailed,
    FileTooLarge,
    // ── process 域 ────────────────────────────────────────────────
    ProcessFailed,
    SpawnFailed,
    StdinFailed,
    StdoutFailed,
    StderrFailed,
    // ── llm 域 ────────────────────────────────────────────────────
    InvalidApiKey,
    ModelNotFound,
    InvalidParameter,
    QuotaExceeded,
    ContextWindowExceeded,
    RateLimited,
    ClientError,
    ServerError,
    StreamDisconnected,
    StreamParse,
    ContentFiltered,
    TokenLimit,
    EmptyResponse,
    ProviderRateLimited,
    LlmStreamError,
    // ── s5r 会话域 ────────────────────────────────────────────────
    InvalidManifest,
    NotInitialized,
    EmitFailed,
    DispatchFailed,
    UnknownParentInvoke,
    ReentrancyExceeded,
    UnsupportedProtocolVersion,
    StreamNotSupported,
    StreamFailed,
    StreamClosed,
    // ── worker（guest）域 ─────────────────────────────────────────
    UnknownHandler,
    DuplicateRegistration,
    UnsupportedHook,
    TypedHookRequired,
    InvalidHookMode,
    InvalidHookRegistration,
    InvalidHttpRoute,
    InvalidArguments,
    PeerStartFailed,
    HostApiAlreadySet,
    ManifestSerializeFailed,
    InitializeFailed,
    HandlerPanicked,
    HostError,
    NestedFailed,
    PeerOverloaded,
    InvalidCapabilityRegistry,
    // ── storage 域 ────────────────────────────────────────────────
    StorageIoError,
    StorageLockError,
    CorruptSessionData,
}

impl WireErrorCode {
    /// 线缆上使用的稳定字符串；协议兼容性要求该映射永不改变。
    pub const fn as_str(&self) -> &'static str {
        match self {
            Self::PermissionDenied => "permission_denied",
            Self::BackendUnavailable => "backend_unavailable",
            Self::ContextUnavailable => "context_unavailable",
            Self::InvalidInput => "invalid_input",
            Self::Cancelled => "cancelled",
            Self::Timeout => "timeout",
            Self::HostNotReady => "host_not_ready",
            Self::PeerBusy => "peer_busy",
            Self::PeerClosed => "peer_closed",
            Self::Transport => "transport_error",
            Self::IoError => "io_error",
            Self::UnknownCapability => "unknown_capability",
            Self::StateTooLarge => "state_too_large",
            Self::SerializationFailed => "serialization_failed",
            Self::InvalidResponse => "invalid_host_response",
            Self::Unsupported => "unsupported",
            Self::NotSupported => "not_supported",
            Self::HostRuntimeFailed => "host_runtime_failed",
            Self::SessionNotFound => "session_not_found",
            Self::SessionBusy => "session_busy",
            Self::SessionAlreadyExists => "session_already_exists",
            Self::MaxDepthExceeded => "max_depth_exceeded",
            Self::InternalError => "internal_error",
            Self::InvalidRequest => "invalid_request",
            Self::DuplicateRequestId => "duplicate_request_id",
            Self::ReadFailed => "read_failed",
            Self::NetworkRequestFailed => "network_request_failed",
            Self::ResponseTooLarge => "response_too_large",
            Self::FileTooLarge => "file_too_large",
            Self::ProcessFailed => "process_failed",
            Self::SpawnFailed => "spawn_failed",
            Self::StdinFailed => "stdin_failed",
            Self::StdoutFailed => "stdout_failed",
            Self::StderrFailed => "stderr_failed",
            Self::InvalidApiKey => "invalid_api_key",
            Self::ModelNotFound => "model_not_found",
            Self::InvalidParameter => "invalid_parameter",
            Self::QuotaExceeded => "quota_exceeded",
            Self::ContextWindowExceeded => "context_window_exceeded",
            Self::RateLimited => "rate_limited",
            Self::ClientError => "client_error",
            Self::ServerError => "server_error",
            Self::StreamDisconnected => "stream_disconnected",
            Self::StreamParse => "stream_parse",
            Self::ContentFiltered => "content_filtered",
            Self::TokenLimit => "token_limit",
            Self::EmptyResponse => "empty_response",
            Self::ProviderRateLimited => "provider_rate_limited",
            Self::LlmStreamError => "llm_stream_error",
            Self::InvalidManifest => "invalid_manifest",
            Self::NotInitialized => "not_initialized",
            Self::EmitFailed => "emit_failed",
            Self::DispatchFailed => "dispatch_failed",
            Self::UnknownParentInvoke => "unknown_parent_invoke",
            Self::ReentrancyExceeded => "reentrancy_exceeded",
            Self::UnsupportedProtocolVersion => "unsupported_protocol_version",
            Self::StreamNotSupported => "stream_not_supported",
            Self::StreamFailed => "stream_failed",
            Self::StreamClosed => "stream_closed",
            Self::UnknownHandler => "unknown_handler",
            Self::DuplicateRegistration => "duplicate_registration",
            Self::UnsupportedHook => "unsupported_hook",
            Self::TypedHookRequired => "typed_hook_required",
            Self::InvalidHookMode => "invalid_hook_mode",
            Self::InvalidHookRegistration => "invalid_hook_registration",
            Self::InvalidHttpRoute => "invalid_http_route",
            Self::InvalidArguments => "invalid_arguments",
            Self::PeerStartFailed => "peer_start_failed",
            Self::HostApiAlreadySet => "host_api_already_set",
            Self::ManifestSerializeFailed => "manifest_serialize_failed",
            Self::InitializeFailed => "initialize_failed",
            Self::HandlerPanicked => "handler_panicked",
            Self::HostError => "host_error",
            Self::NestedFailed => "nested_failed",
            Self::PeerOverloaded => "peer_overloaded",
            Self::InvalidCapabilityRegistry => "invalid_capability_registry",
            Self::StorageIoError => "storage_io_error",
            Self::StorageLockError => "storage_lock_error",
            Self::CorruptSessionData => "corrupt_session_data",
        }
    }

    /// 解析线缆字符串；未知码返回 `None`（旧宿主/新扩展的码由调用方透传）。
    pub fn parse(code: &str) -> Option<Self> {
        Some(match code {
            "permission_denied" => Self::PermissionDenied,
            "backend_unavailable" => Self::BackendUnavailable,
            "context_unavailable" => Self::ContextUnavailable,
            "invalid_input" => Self::InvalidInput,
            "cancelled" => Self::Cancelled,
            "timeout" => Self::Timeout,
            "host_not_ready" => Self::HostNotReady,
            "peer_busy" => Self::PeerBusy,
            "peer_closed" => Self::PeerClosed,
            "transport_error" => Self::Transport,
            "io_error" => Self::IoError,
            "unknown_capability" => Self::UnknownCapability,
            "state_too_large" => Self::StateTooLarge,
            "serialization_failed" => Self::SerializationFailed,
            "invalid_host_response" => Self::InvalidResponse,
            "unsupported" => Self::Unsupported,
            "not_supported" => Self::NotSupported,
            "host_runtime_failed" => Self::HostRuntimeFailed,
            "session_not_found" => Self::SessionNotFound,
            "session_busy" => Self::SessionBusy,
            "session_already_exists" => Self::SessionAlreadyExists,
            "max_depth_exceeded" => Self::MaxDepthExceeded,
            "internal_error" => Self::InternalError,
            "invalid_request" => Self::InvalidRequest,
            "duplicate_request_id" => Self::DuplicateRequestId,
            "read_failed" => Self::ReadFailed,
            "network_request_failed" => Self::NetworkRequestFailed,
            "response_too_large" => Self::ResponseTooLarge,
            "file_too_large" => Self::FileTooLarge,
            "process_failed" => Self::ProcessFailed,
            "spawn_failed" => Self::SpawnFailed,
            "stdin_failed" => Self::StdinFailed,
            "stdout_failed" => Self::StdoutFailed,
            "stderr_failed" => Self::StderrFailed,
            "invalid_api_key" => Self::InvalidApiKey,
            "model_not_found" => Self::ModelNotFound,
            "invalid_parameter" => Self::InvalidParameter,
            "quota_exceeded" => Self::QuotaExceeded,
            "context_window_exceeded" => Self::ContextWindowExceeded,
            "rate_limited" => Self::RateLimited,
            "client_error" => Self::ClientError,
            "server_error" => Self::ServerError,
            "stream_disconnected" => Self::StreamDisconnected,
            "stream_parse" => Self::StreamParse,
            "content_filtered" => Self::ContentFiltered,
            "token_limit" => Self::TokenLimit,
            "empty_response" => Self::EmptyResponse,
            "provider_rate_limited" => Self::ProviderRateLimited,
            "llm_stream_error" => Self::LlmStreamError,
            "invalid_manifest" => Self::InvalidManifest,
            "not_initialized" => Self::NotInitialized,
            "emit_failed" => Self::EmitFailed,
            "dispatch_failed" => Self::DispatchFailed,
            "unknown_parent_invoke" => Self::UnknownParentInvoke,
            "reentrancy_exceeded" => Self::ReentrancyExceeded,
            "unsupported_protocol_version" => Self::UnsupportedProtocolVersion,
            "stream_not_supported" => Self::StreamNotSupported,
            "stream_failed" => Self::StreamFailed,
            "stream_closed" => Self::StreamClosed,
            "unknown_handler" => Self::UnknownHandler,
            "duplicate_registration" => Self::DuplicateRegistration,
            "unsupported_hook" => Self::UnsupportedHook,
            "typed_hook_required" => Self::TypedHookRequired,
            "invalid_hook_mode" => Self::InvalidHookMode,
            "invalid_hook_registration" => Self::InvalidHookRegistration,
            "invalid_http_route" => Self::InvalidHttpRoute,
            "invalid_arguments" => Self::InvalidArguments,
            "peer_start_failed" => Self::PeerStartFailed,
            "host_api_already_set" => Self::HostApiAlreadySet,
            "manifest_serialize_failed" => Self::ManifestSerializeFailed,
            "initialize_failed" => Self::InitializeFailed,
            "handler_panicked" => Self::HandlerPanicked,
            "host_error" => Self::HostError,
            "nested_failed" => Self::NestedFailed,
            "peer_overloaded" => Self::PeerOverloaded,
            "invalid_capability_registry" => Self::InvalidCapabilityRegistry,
            "storage_io_error" => Self::StorageIoError,
            "storage_lock_error" => Self::StorageLockError,
            "corrupt_session_data" => Self::CorruptSessionData,
            _ => return None,
        })
    }
}

impl Display for WireErrorCode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl From<WireErrorCode> for String {
    fn from(code: WireErrorCode) -> Self {
        code.as_str().to_owned()
    }
}

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

    /// 所有 variant 的枚举清单：as_str 与 parse 必须双射，且 wire 字符串稳定。
    const ALL_CODES: &[WireErrorCode] = &[
        WireErrorCode::PermissionDenied,
        WireErrorCode::BackendUnavailable,
        WireErrorCode::ContextUnavailable,
        WireErrorCode::InvalidInput,
        WireErrorCode::Cancelled,
        WireErrorCode::Timeout,
        WireErrorCode::HostNotReady,
        WireErrorCode::PeerBusy,
        WireErrorCode::PeerClosed,
        WireErrorCode::Transport,
        WireErrorCode::IoError,
        WireErrorCode::UnknownCapability,
        WireErrorCode::StateTooLarge,
        WireErrorCode::SerializationFailed,
        WireErrorCode::InvalidResponse,
        WireErrorCode::Unsupported,
        WireErrorCode::NotSupported,
        WireErrorCode::HostRuntimeFailed,
        WireErrorCode::SessionNotFound,
        WireErrorCode::SessionBusy,
        WireErrorCode::SessionAlreadyExists,
        WireErrorCode::MaxDepthExceeded,
        WireErrorCode::InternalError,
        WireErrorCode::InvalidRequest,
        WireErrorCode::DuplicateRequestId,
        WireErrorCode::ReadFailed,
        WireErrorCode::FileTooLarge,
        WireErrorCode::ProcessFailed,
        WireErrorCode::SpawnFailed,
        WireErrorCode::StdinFailed,
        WireErrorCode::StdoutFailed,
        WireErrorCode::StderrFailed,
        WireErrorCode::NetworkRequestFailed,
        WireErrorCode::ResponseTooLarge,
        WireErrorCode::InvalidApiKey,
        WireErrorCode::ModelNotFound,
        WireErrorCode::InvalidParameter,
        WireErrorCode::QuotaExceeded,
        WireErrorCode::ContextWindowExceeded,
        WireErrorCode::RateLimited,
        WireErrorCode::ClientError,
        WireErrorCode::ServerError,
        WireErrorCode::StreamDisconnected,
        WireErrorCode::StreamParse,
        WireErrorCode::ContentFiltered,
        WireErrorCode::TokenLimit,
        WireErrorCode::EmptyResponse,
        WireErrorCode::ProviderRateLimited,
        WireErrorCode::LlmStreamError,
        WireErrorCode::InvalidManifest,
        WireErrorCode::NotInitialized,
        WireErrorCode::EmitFailed,
        WireErrorCode::DispatchFailed,
        WireErrorCode::UnknownParentInvoke,
        WireErrorCode::ReentrancyExceeded,
        WireErrorCode::UnsupportedProtocolVersion,
        WireErrorCode::StreamNotSupported,
        WireErrorCode::StreamFailed,
        WireErrorCode::StreamClosed,
        WireErrorCode::UnknownHandler,
        WireErrorCode::DuplicateRegistration,
        WireErrorCode::UnsupportedHook,
        WireErrorCode::TypedHookRequired,
        WireErrorCode::InvalidHookMode,
        WireErrorCode::InvalidHookRegistration,
        WireErrorCode::InvalidHttpRoute,
        WireErrorCode::InvalidArguments,
        WireErrorCode::PeerStartFailed,
        WireErrorCode::HostApiAlreadySet,
        WireErrorCode::ManifestSerializeFailed,
        WireErrorCode::InitializeFailed,
        WireErrorCode::HandlerPanicked,
        WireErrorCode::HostError,
        WireErrorCode::NestedFailed,
        WireErrorCode::PeerOverloaded,
        WireErrorCode::InvalidCapabilityRegistry,
        WireErrorCode::StorageIoError,
        WireErrorCode::StorageLockError,
        WireErrorCode::CorruptSessionData,
    ];

    #[test]
    fn every_wire_code_round_trips_through_parse() {
        for code in ALL_CODES {
            let wire = code.as_str();
            assert_eq!(
                WireErrorCode::parse(wire),
                Some(*code),
                "as_str/parse must be a bijection for {wire}"
            );
        }
    }

    #[test]
    fn wire_strings_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for code in ALL_CODES {
            assert!(
                seen.insert(code.as_str()),
                "duplicate wire string {}",
                code.as_str()
            );
        }
    }

    #[test]
    fn unknown_wire_strings_parse_to_none() {
        assert_eq!(WireErrorCode::parse("future_host_failure"), None);
        assert_eq!(WireErrorCode::parse(""), None);
    }

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
