//! 小模型 ID 能力。
//!
//! 允许扩展获取当前配置的小模型标识，
//! 用于子 agent 的模型选择等场景。

use std::sync::Arc;

use super::Capability;

/// 小模型 ID 能力的 newtype 包装。
///
/// 扩展通过 `ctx.get_capability::<SmallModelIdCap>()` 获取，
/// 然后调用 `cap.small_model_id()` 拿到模型标识。
pub struct SmallModelIdCap(Arc<dyn SmallModelIdInner>);

impl SmallModelIdCap {
    pub fn new(inner: Arc<dyn SmallModelIdInner>) -> Self {
        Self(inner)
    }
}

impl Capability for SmallModelIdCap {}

impl SmallModelIdCap {
    /// 返回当前配置的小模型标识。
    pub fn small_model_id(&self) -> String {
        self.0.small_model_id()
    }
}

impl std::fmt::Debug for SmallModelIdCap {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SmallModelIdCap").finish()
    }
}

/// 小模型 ID 的能力接口。由宿主侧实现。
pub trait SmallModelIdInner: Send + Sync + 'static {
    /// 返回当前配置的小模型标识。
    fn small_model_id(&self) -> String;
}
