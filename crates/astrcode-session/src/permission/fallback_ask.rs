use astrcode_core::permission::{PermissionContext, PermissionDecision, PermissionPolicy};

pub struct FallbackAskPolicy;

impl PermissionPolicy for FallbackAskPolicy {
    fn priority(&self) -> u32 {
        999
    }

    fn evaluate(&self, _ctx: &PermissionContext<'_>) -> PermissionDecision {
        // manual 模式兜底：未命中更具体策略的工具默认放行。
        // shell / 敏感路径等由更高优先级策略单独 Ask。
        PermissionDecision::Pass
    }
}
