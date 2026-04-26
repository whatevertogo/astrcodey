//! 工具管理相关 DTO
//!
//! 定义工具摘要查询和工具执行接口的请求/响应结构。
//! 这些 DTO 用于前端展示可用工具列表和触发工具执行请求。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 对外暴露的工具摘要。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolDescriptorDto {
    pub name: String,
    pub description: String,
    pub profiles: Vec<String>,
    pub streaming: bool,
}

/// `POST /api/v1/tools/{id}/execute` 请求体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecuteRequestDto {
    pub input: Value,
}

/// `POST /api/v1/tools/{id}/execute` 响应体。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ToolExecuteResponseDto {
    pub accepted: bool,
    pub message: String,
}
