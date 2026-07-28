use astrcode_core::types::SessionId;

/// Server 回收 session 时清理关联外部资源的幂等接口。
pub trait SessionResourceCleanup: Send + Sync {
    fn cleanup(&self, session_id: &SessionId);
}
