use astrcode_core::permission::{
    ApprovalMode, PermissionContext, PermissionDecision, PermissionPolicy,
};

pub struct YoloModeApprovePolicy;

impl PermissionPolicy for YoloModeApprovePolicy {
    fn priority(&self) -> u32 {
        50
    }

    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PermissionDecision {
        if ctx.approval_mode == ApprovalMode::Yolo {
            PermissionDecision::Allow
        } else {
            PermissionDecision::Pass
        }
    }
}
