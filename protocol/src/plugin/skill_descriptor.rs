//! 插件 Skill 声明协议
//!
//! 定义插件向 host 声明 skill 的稳定 DTO 结构。
//!
//! ## 设计意图
//!
//! 插件可以通过握手阶段的 `InitializeResultData.skills` 字段声明自己提供的 skill。
//! Host 将这些声明解析为 `SkillSpec`，并统一纳入 `SkillCatalog` 管理。
//!
//! ## 资产物化
//!
//! 插件 skill 的资产文件（`assets`）会在初始化时被物化到 runtime 缓存目录，
//! 使得 `Skill` tool 能以统一的方式访问 builtin/user/project/plugin skill 的资源。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Skill 资产文件描述符。
///
/// 插件 skill 可以附带参考文档、脚本等资产文件，
/// 这些文件会被物化到 runtime 缓存目录供运行时访问。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SkillAssetDescriptor {
    /// 资产文件相对于 skill 根目录的路径（如 `references/api.md`）。
    pub relative_path: String,
    /// 文件内容。
    pub content: String,
    /// 内容编码方式，目前仅支持 `"utf-8"`。
    #[serde(default = "default_encoding")]
    pub encoding: String,
}

fn default_encoding() -> String {
    "utf-8".to_string()
}

/// Skill 声明描述符。
///
/// 插件在握手阶段通过此结构声明自己提供的 skill。
/// Host 将其转换为内部的 `SkillSpec`，来源标记为 `Plugin` 或 `Mcp`。
///
/// ## 字段说明
///
/// - `name`: skill 的唯一标识，必须为 kebab-case，与 skill 文件夹名一致
/// - `description`: 简短描述，用于 system prompt 索引
/// - `guide`: 完整的 skill 指南正文（markdown 格式）
/// - `allowed_tools`: 此 skill 允许调用的工具列表（可选）
/// - `assets`: 附带的资产文件列表（可选）
/// - `metadata`: 扩展元数据（可选）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SkillDescriptor {
    /// Skill 的唯一标识（kebab-case，必须与 skill 文件夹名一致）。
    pub name: String,
    /// Skill 的简短描述，用于 system prompt 中的索引展示。
    pub description: String,
    /// Skill 的完整指南正文（markdown 格式）。
    pub guide: String,
    /// 此 skill 允许调用的工具列表。
    ///
    /// 用于限制 skill 执行时的能力边界。如果为空，表示不限制。
    #[serde(default)]
    pub allowed_tools: Vec<String>,
    /// 附带的资产文件列表。
    ///
    /// 这些文件会被物化到 runtime 缓存目录，供 `Skill` tool 运行时访问。
    #[serde(default)]
    pub assets: Vec<SkillAssetDescriptor>,
    /// 扩展元数据。
    #[serde(default)]
    pub metadata: Value,
}
