//! Prompt 组装的最终计划。
//!
//! [`PromptPlan`] 是 [`PromptComposer::build`](crate::composer::PromptComposer::build)
//! 的核心产出物，包含已渲染的 system blocks、prepend/append 消息和额外工具定义。
//!
//! # 渲染流程
//!
//! `render_system()` 将 system blocks 按优先级排序后拼接为完整的 system prompt 字符串。
//! prepend/append 消息则直接作为 LLM 对话消息的一部分。

use astrcode_core::LlmMessage;
use astrcode_runtime_contract::tool::ToolDefinition;
use serde::Serialize;

use super::{PromptBlock, append_unique_tools, block::PromptLayer};

/// 已渲染 system block 的来源摘要。
///
/// 这是 `PromptPlan` 对外暴露的稳定来源投影，调用方只依赖最终渲染结果，
/// 不需要再回看 contributor 或 declaration 内部结构。
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PromptPlanSourceMetadata {
    pub block_id: String,
    pub title: String,
    pub layer: PromptLayer,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub category: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

/// Prompt 组装的最终计划。
///
/// 由 composer 经过收集、去重、条件过滤、依赖解析、模板渲染后生成。
/// 调用方（通常是 `runtime` crate）将此计划转换为实际的 LLM API 请求。
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub struct PromptPlan {
    pub system_blocks: Vec<PromptBlock>,
    pub prepend_messages: Vec<LlmMessage>,
    pub append_messages: Vec<LlmMessage>,
    pub extra_tools: Vec<ToolDefinition>,
}

impl PromptPlan {
    /// 以最终渲染顺序返回 system blocks。
    pub fn ordered_system_blocks(&self) -> Vec<&PromptBlock> {
        let mut blocks: Vec<&PromptBlock> = self.system_blocks.iter().collect();
        blocks.sort_by_key(|block| {
            (
                prompt_layer_rank(block.layer),
                block.priority,
                block.insertion_order,
            )
        });
        blocks
    }

    /// 将 system blocks 渲染为完整的 system prompt 字符串。
    ///
    /// Blocks 按 `(priority, insertion_order)` 排序后，以 `[Title]\ncontent` 格式拼接。
    /// 如果没有 system blocks，返回 `None`。
    pub fn render_system(&self) -> Option<String> {
        if self.system_blocks.is_empty() {
            return None;
        }

        Some(
            self.ordered_system_blocks()
                .into_iter()
                .map(|block| format!("[{}]\n{}", block.title, block.content))
                .collect::<Vec<_>>()
                .join("\n\n"),
        )
    }

    /// 返回最终渲染后的 system block 来源信息。
    pub fn source_metadata(&self) -> Vec<PromptPlanSourceMetadata> {
        self.ordered_system_blocks()
            .into_iter()
            .map(|block| PromptPlanSourceMetadata {
                block_id: block.id.clone(),
                title: block.title.clone(),
                layer: block.layer,
                category: block
                    .metadata
                    .category
                    .as_ref()
                    .map(|value| value.to_string()),
                source: block.metadata.source_name().map(str::to_string),
                origin: block.metadata.origin.clone(),
            })
            .collect()
    }

    /// 以指定层级合并另一个 plan。
    ///
    /// 这里显式重写 insertion_order，是为了保证不同 layer 单独 build 后再 merge 时，
    /// 全局排序仍然稳定且单调递增。
    pub fn extend_with_layer(&mut self, other: PromptPlan, layer: PromptLayer) {
        let insertion_offset = self
            .system_blocks
            .iter()
            .map(|block| block.insertion_order)
            .max()
            .map(|value| value.saturating_add(1))
            .unwrap_or(0);

        self.system_blocks
            .extend(other.system_blocks.into_iter().map(|block| {
                let order = block.insertion_order + insertion_offset;
                let layer = match block.layer {
                    PromptLayer::Unspecified => layer,
                    explicit => explicit,
                };
                block.with_insertion_order(order).with_layer(layer)
            }));
        self.prepend_messages.extend(other.prepend_messages);
        self.append_messages.extend(other.append_messages);
        append_unique_tools(&mut self.extra_tools, other.extra_tools);
    }
}

fn prompt_layer_rank(layer: PromptLayer) -> u8 {
    match layer {
        PromptLayer::Stable => 0,
        PromptLayer::SemiStable => 1,
        PromptLayer::Inherited => 2,
        PromptLayer::Dynamic => 3,
        PromptLayer::Unspecified => 255,
    }
}

trait PromptBlockExt {
    fn with_insertion_order(self, insertion_order: usize) -> Self;
}

impl PromptBlockExt for PromptBlock {
    fn with_insertion_order(mut self, insertion_order: usize) -> Self {
        self.insertion_order = insertion_order;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BlockKind, PromptBlock, block::BlockMetadata};

    #[test]
    fn render_system_returns_none_when_empty() {
        assert!(PromptPlan::default().render_system().is_none());
    }

    #[test]
    fn render_system_formats_single_block() {
        let plan = PromptPlan {
            system_blocks: vec![PromptBlock::new(
                "identity",
                BlockKind::Identity,
                "Identity",
                "hello",
                100,
                BlockMetadata::default(),
                0,
            )],
            ..PromptPlan::default()
        };

        assert_eq!(plan.render_system().as_deref(), Some("[Identity]\nhello"));
    }

    #[test]
    fn render_system_sorts_blocks_by_priority_and_insertion_order() {
        let plan = PromptPlan {
            system_blocks: vec![
                PromptBlock::new(
                    "project",
                    BlockKind::ProjectRules,
                    "Project Rules",
                    "project",
                    500,
                    BlockMetadata::default(),
                    2,
                ),
                PromptBlock::new(
                    "identity",
                    BlockKind::Identity,
                    "Identity",
                    "identity",
                    100,
                    BlockMetadata::default(),
                    1,
                ),
                PromptBlock::new(
                    "environment",
                    BlockKind::Environment,
                    "Environment",
                    "environment",
                    100,
                    BlockMetadata::default(),
                    0,
                ),
            ],
            ..PromptPlan::default()
        };

        let rendered = plan.render_system().expect("system prompt should render");

        assert_eq!(
            rendered,
            "[Environment]\nenvironment\n\n[Identity]\nidentity\n\n[Project Rules]\nproject"
        );
    }

    #[test]
    fn source_metadata_tracks_rendered_block_sources() {
        let plan = PromptPlan {
            system_blocks: vec![
                PromptBlock::new(
                    "governance.collaboration.guide",
                    BlockKind::ExtensionInstruction,
                    "Child Agent Collaboration Guide",
                    "guide",
                    590,
                    BlockMetadata {
                        tags: vec!["source:builtin".into()],
                        category: Some("extensions".into()),
                        origin: Some("governance:collaboration-guide".to_string()),
                    },
                    1,
                )
                .with_layer(PromptLayer::Dynamic),
                PromptBlock::new(
                    "child.execution.contract",
                    BlockKind::ExtensionInstruction,
                    "Child Execution Contract",
                    "contract",
                    585,
                    BlockMetadata {
                        tags: vec!["source:builtin".into()],
                        category: Some("extensions".into()),
                        origin: Some("child-contract:fresh".to_string()),
                    },
                    0,
                )
                .with_layer(PromptLayer::Inherited),
            ],
            ..PromptPlan::default()
        };

        assert_eq!(
            plan.source_metadata(),
            vec![
                PromptPlanSourceMetadata {
                    block_id: "child.execution.contract".to_string(),
                    title: "Child Execution Contract".to_string(),
                    layer: PromptLayer::Inherited,
                    category: Some("extensions".to_string()),
                    source: Some("builtin".to_string()),
                    origin: Some("child-contract:fresh".to_string()),
                },
                PromptPlanSourceMetadata {
                    block_id: "governance.collaboration.guide".to_string(),
                    title: "Child Agent Collaboration Guide".to_string(),
                    layer: PromptLayer::Dynamic,
                    category: Some("extensions".to_string()),
                    source: Some("builtin".to_string()),
                    origin: Some("governance:collaboration-guide".to_string()),
                },
            ]
        );
    }
}
