use std::fmt::{Display, Formatter};

macro_rules! wire_error_codes {
    ($($variant:ident => $wire:literal),+ $(,)?) => {
        /// Known S5R error codes. Wire strings are permanent and must never be reused.
        /// Invalid caller values use `InvalidInput`; unsupported operations use `Unsupported`.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
        #[non_exhaustive]
        pub enum WireErrorCode {
            $($variant),+
        }

        impl WireErrorCode {
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            pub const fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $wire),+
                }
            }

            pub fn parse(code: &str) -> Option<Self> {
                match code {
                    $($wire => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

wire_error_codes! {
    PermissionDenied => "permission_denied",
    BackendUnavailable => "backend_unavailable",
    ContextUnavailable => "context_unavailable",
    InvalidInput => "invalid_input",
    Cancelled => "cancelled",
    Timeout => "timeout",
    HostNotReady => "host_not_ready",
    PeerBusy => "peer_busy",
    PeerClosed => "peer_closed",
    Transport => "transport_error",
    IoError => "io_error",
    UnknownCapability => "unknown_capability",
    StateTooLarge => "state_too_large",
    SerializationFailed => "serialization_failed",
    InvalidResponse => "invalid_host_response",
    Unsupported => "unsupported",
    HostRuntimeFailed => "host_runtime_failed",
    ExtensionDraining => "extension_draining",
    UnsupportedFeature => "unsupported_feature",
    NetworkRequestFailed => "network_request_failed",
    ResponseTooLarge => "response_too_large",
    SessionNotFound => "session_not_found",
    SessionBusy => "session_busy",
    SessionAlreadyExists => "session_already_exists",
    MaxDepthExceeded => "max_depth_exceeded",
    InternalError => "internal_error",
    InvalidRequest => "invalid_request",
    DuplicateRequestId => "duplicate_request_id",
    ReadFailed => "read_failed",
    StaleFile => "stale_file",
    FileTooLarge => "file_too_large",
    ProcessFailed => "process_failed",
    SpawnFailed => "spawn_failed",
    StdinFailed => "stdin_failed",
    StdoutFailed => "stdout_failed",
    StderrFailed => "stderr_failed",
    InvalidApiKey => "invalid_api_key",
    ModelNotFound => "model_not_found",
    QuotaExceeded => "quota_exceeded",
    ContextWindowExceeded => "context_window_exceeded",
    RateLimited => "rate_limited",
    ClientError => "client_error",
    ServerError => "server_error",
    StreamDisconnected => "stream_disconnected",
    StreamParse => "stream_parse",
    ContentFiltered => "content_filtered",
    TokenLimit => "token_limit",
    EmptyResponse => "empty_response",
    LlmStreamError => "llm_stream_error",
    EmitFailed => "emit_failed",
    DispatchFailed => "dispatch_failed",
    UnknownParentInvoke => "unknown_parent_invoke",
    ReentrancyExceeded => "reentrancy_exceeded",
    UnsupportedProtocolVersion => "unsupported_protocol_version",
    StreamNotSupported => "stream_not_supported",
    StreamClosed => "stream_closed",
    BackpressureTimeout => "backpressure_timeout",
    StreamIdleTimeout => "stream_idle_timeout",
    UnknownHandler => "unknown_handler",
    DuplicateRegistration => "duplicate_registration",
    UnsupportedHook => "unsupported_hook",
    TypedHookRequired => "typed_hook_required",
    InvalidHookMode => "invalid_hook_mode",
    InvalidHookRegistration => "invalid_hook_registration",
    InvalidHttpRoute => "invalid_http_route",
    NestedFailed => "nested_failed",
    PeerOverloaded => "peer_overloaded",
    InvalidCapabilityRegistry => "invalid_capability_registry",
    StorageIoError => "storage_io_error",
    StorageLockError => "storage_lock_error",
    CorruptSessionData => "corrupt_session_data",
}

impl Display for WireErrorCode {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use super::WireErrorCode;

    #[test]
    fn catalog_is_unique_and_round_trips_while_unknown_codes_remain_unknown() {
        let mut strings = HashSet::new();
        for code in WireErrorCode::ALL {
            assert!(strings.insert(code.as_str()), "duplicate code: {code}");
            assert_eq!(WireErrorCode::parse(code.as_str()), Some(*code));
        }
        assert_eq!(WireErrorCode::parse("future_remote_failure"), None);
    }
}
