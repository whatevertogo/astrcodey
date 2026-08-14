use astrcode_core::permission::ApprovalMode;

use super::{PermissionContext, PermissionPolicy, PolicyDecision};

pub(super) struct YoloModeApprovePolicy;

impl PermissionPolicy for YoloModeApprovePolicy {
    fn evaluate(&self, ctx: &PermissionContext<'_>) -> PolicyDecision {
        if ctx.approval_mode == ApprovalMode::Yolo {
            PolicyDecision::Allow
        } else {
            PolicyDecision::Pass
        }
    }
}
