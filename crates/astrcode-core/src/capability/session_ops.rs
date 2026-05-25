//! 会话操作能力。

use std::sync::Arc;

use crate::tool::{
    CreateSessionRequest, SessionApiError, SessionHandle, SessionStatus, SubmitTurnRequest,
    SubmitTurnResult,
};

use super::Capability;

/// 会话操作能力的 newtype 包装。
///
/// 消费侧通过 `ctx.get_capability::<SessionOpsCap>()` 获取，
/// 然后调用 `cap.create_session(...)` 等方法。
///
/// # 为什么用 newtype？
///
/// `dyn SessionOpsInner` 不满足 `Sized`，无法作为 `TypeId` 泛型参数。
/// newtype `SessionOpsCap` 是 `Sized + 'static`，完美满足 `Capability` 要求。
pub struct SessionOpsCap(pub(crate) Arc<dyn SessionOpsInner>);

impl SessionOpsCap {
    pub fn new(inner: Arc<dyn SessionOpsInner>) -> Self {
        Self(inner)
    }
}

impl Capability for SessionOpsCap {}

impl SessionOpsCap {
    pub async fn create_session(
        &self,
        parent_session_id: &str,
        request: CreateSessionRequest,
    ) -> Result<SessionHandle, SessionApiError> {
        self.0.create_session(parent_session_id, request).await
    }

    pub async fn submit_turn(
        &self,
        caller_session_id: &str,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResult, SessionApiError> {
        self.0.submit_turn(caller_session_id, request).await
    }

    pub async fn recycle_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<(), SessionApiError> {
        self.0.recycle_session(caller_session_id, target_session_id).await
    }

    pub async fn query_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<SessionStatus, SessionApiError> {
        self.0.query_session(caller_session_id, target_session_id).await
    }
}

impl std::fmt::Debug for SessionOpsCap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionOpsCap").finish()
    }
}

/// 会话操作的能力接口。由 server 层实现。
///
/// 方法签名与 [`crate::tool::SessionOperations`] 一一对应，
/// 但不要求实现方同时实现 `SessionOperations`——
/// server 侧可以委托给已有的 `SessionOperations` 实现。
#[async_trait::async_trait]
pub trait SessionOpsInner: Send + Sync + 'static {
    async fn create_session(
        &self,
        parent_session_id: &str,
        request: CreateSessionRequest,
    ) -> Result<SessionHandle, SessionApiError>;

    async fn submit_turn(
        &self,
        caller_session_id: &str,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResult, SessionApiError>;

    async fn recycle_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<(), SessionApiError>;

    async fn query_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<SessionStatus, SessionApiError>;
}

// ─── SessionOperations → SessionOpsInner Adapter ───────────────────────

/// 将已有的 [`crate::tool::SessionOperations`] 适配为 [`SessionOpsInner`]。
///
/// 用于过渡期：`ToolCapabilities.session_ops` 仍然持有 `Arc<dyn SessionOperations>`，
/// 此适配器允许将其包装为 `SessionOpsCap` 注入 `CapabilityRegistry`，
/// 无需修改 server 侧的绑定逻辑。
struct SessionOpsAdapter(Arc<dyn crate::tool::SessionOperations>);

#[async_trait::async_trait]
impl SessionOpsInner for SessionOpsAdapter {
    async fn create_session(
        &self,
        parent_session_id: &str,
        request: CreateSessionRequest,
    ) -> Result<SessionHandle, SessionApiError> {
        self.0.create_session(parent_session_id, request).await
    }

    async fn submit_turn(
        &self,
        caller_session_id: &str,
        request: SubmitTurnRequest,
    ) -> Result<SubmitTurnResult, SessionApiError> {
        self.0.submit_turn(caller_session_id, request).await
    }

    async fn recycle_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<(), SessionApiError> {
        self.0.recycle_session(caller_session_id, target_session_id).await
    }

    async fn query_session(
        &self,
        caller_session_id: &str,
        target_session_id: &str,
    ) -> Result<SessionStatus, SessionApiError> {
        self.0.query_session(caller_session_id, target_session_id).await
    }
}

impl SessionOpsCap {
    /// 从已有的 `SessionOperations` 实例创建 `SessionOpsCap`。
    ///
    /// 过渡期方法——当 `ToolCapabilities.session_ops` 字段删除后，
    /// 应改用 `SessionOpsInner` 直接实现。
    pub fn from_session_ops(ops: Arc<dyn crate::tool::SessionOperations>) -> Self {
        Self::new(Arc::new(SessionOpsAdapter(ops)))
    }
}
