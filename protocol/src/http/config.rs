//! 配置管理相关 DTO
//!
//! 定义配置查看、保存、连接测试的请求/响应结构。
//! 配置数据包括 profile（提供商配置）、活跃模型选择等。

pub use astrcode_core::TestConnectionResult as TestResultDto;
use serde::{Deserialize, Serialize};

use crate::http::RuntimeStatusDto;

/// 配置文件中单个 profile 的只读视图。
///
/// 用于 `GET /api/config` 响应中返回每个 profile 的摘要信息。
/// `api_key_preview` 仅包含密钥的部分字符，避免完整暴露。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ProfileView {
    /// 配置文件中的 profile 名称
    pub name: String,
    /// API 基础 URL
    pub base_url: String,
    /// API 密钥的部分预览（脱敏显示）
    pub api_key_preview: String,
    /// 此 profile 下可用的模型 ID 列表
    pub models: Vec<String>,
}

/// 全局配置的只读视图。
///
/// 用于 `GET /api/config` 响应，返回当前活跃配置摘要。
/// `warning` 字段在配置存在问题时（如 profile 不存在、模型不可用）提供警告信息。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigView {
    /// 配置文件在磁盘上的绝对路径
    pub config_path: String,
    /// 当前活跃的 profile 名称
    pub active_profile: String,
    /// 当前活跃的模型 ID
    pub active_model: String,
    /// 所有已配置的 profile 列表
    pub profiles: Vec<ProfileView>,
    /// 配置警告信息（如 profile 不存在、模型不可用等），无问题时为 None
    pub warning: Option<String>,
}

/// `POST /api/config/reload` 响应体——重新加载后的配置视图。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ConfigReloadResponse {
    /// 重载完成时间（ISO 8601）
    pub reloaded_at: String,
    /// 重载后的配置快照
    pub config: ConfigView,
    /// 重载后已生效的运行时治理快照
    pub status: RuntimeStatusDto,
}

/// `PUT /api/config/selection` 请求体——保存用户的活跃 profile 和模型选择。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SaveActiveSelectionRequest {
    /// 要设为活跃的 profile 名称
    pub active_profile: String,
    /// 要设为活跃的模型 ID
    pub active_model: String,
}

/// `POST /api/config/test-connection` 请求体——测试指定 profile 和模型的连接。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TestConnectionRequest {
    /// 要测试的 profile 名称
    pub profile_name: String,
    /// 要测试的模型 ID
    pub model: String,
}
