//! 输入候选（composer options）相关 DTO。
//!
//! 单个候选项由 host-session 拥有；协议层保留同构 wire DTO，
//! 避免 protocol 反向依赖 runtime owner crate。

use serde::{Deserialize, Serialize};

/// 输入候选项的来源类别。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ComposerOptionKindDto {
    Command,
    Skill,
    Capability,
}

/// 输入候选项被选择后的动作类型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum ComposerOptionActionKindDto {
    InsertText,
    ExecuteCommand,
}

/// 单个输入候选项的 wire DTO。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ComposerOptionDto {
    pub kind: ComposerOptionKindDto,
    pub id: String,
    pub title: String,
    pub description: String,
    pub insert_text: String,
    pub action_kind: ComposerOptionActionKindDto,
    pub action_value: String,
    #[serde(default)]
    pub badges: Vec<String>,
    #[serde(default)]
    pub keywords: Vec<String>,
}

/// 输入候选列表响应。
///
/// 预留响应外层对象而非直接返回数组，是为了后续在不破坏协议的前提下
/// 增加服务端元数据（例如 query 规范化结果或分页信息）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ComposerOptionsResponseDto {
    pub items: Vec<ComposerOptionDto>,
}
