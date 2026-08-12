//! Tool Gate 权限类型：审批模式、策略决策、用户审批决议。

use serde::{Deserialize, Serialize};

/// 工具审批模式（全局配置）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    /// 默认：敏感操作需用户确认。
    #[default]
    Manual,
    /// 跳过 Ask 类策略，自动 Allow。
    Yolo,
}

impl ApprovalMode {
    pub fn parse(raw: &str) -> Option<Self> {
        raw.parse().ok()
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Yolo => "yolo",
        }
    }
}

impl std::str::FromStr for ApprovalMode {
    type Err = ();

    fn from_str(raw: &str) -> Result<Self, Self::Err> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "manual" => Ok(Self::Manual),
            "yolo" => Ok(Self::Yolo),
            _ => Err(()),
        }
    }
}

/// 用户对挂起审批的决议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    AllowOnce,
    DenyOnce,
    AllowAlways,
    DenyAlways,
}

impl ApprovalDecision {
    pub fn allows(&self) -> bool {
        matches!(self, Self::AllowOnce | Self::AllowAlways)
    }
}

/// 审批请求来源（扩展 PreToolUse::Ask 或 session 权限链）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalSource {
    Extension,
    Core,
}

/// 用户配置的权限规则（第三期 DSL）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionsSection {
    #[serde(default)]
    pub deny: Vec<PermissionRule>,
    #[serde(default)]
    pub ask: Vec<PermissionRule>,
    #[serde(default)]
    pub allow: Vec<PermissionRule>,
}

/// 单条权限规则。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PermissionRule {
    pub tool: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::ApprovalMode;

    #[test]
    fn approval_mode_parsing_is_case_insensitive_and_closed() {
        assert_eq!("yolo".parse(), Ok(ApprovalMode::Yolo));
        assert_eq!("MANUAL".parse(), Ok(ApprovalMode::Manual));
        assert!("unknown".parse::<ApprovalMode>().is_err());
    }
}
