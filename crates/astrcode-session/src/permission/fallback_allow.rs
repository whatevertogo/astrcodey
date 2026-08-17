use super::{PermissionContext, PermissionPolicy, PolicyDecision};

/// manual 模式兜底：未命中更具体策略的工具默认放行。
///
/// shell / 敏感路径等由更高优先级策略单独 Ask。
pub(super) struct FallbackAllowPolicy;

impl PermissionPolicy for FallbackAllowPolicy {
    fn evaluate(&self, _ctx: &PermissionContext<'_>) -> PolicyDecision {
        PolicyDecision::Allow
    }
}
