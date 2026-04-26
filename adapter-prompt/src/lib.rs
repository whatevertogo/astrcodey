//! Prompt 组装库。
//!
//! # 架构概述
//!
//! 本 crate 实现了 agent 循环中的 prompt 组装管线，采用**贡献者模式**（contributor pattern）：
//! 每个 [`PromptContributor`] 负责生成一段 prompt 内容（称为 [`BlockSpec`]），
//! [`PromptComposer`] 收集所有贡献、解析依赖、渲染模板，最终产出 [`PromptPlan`]。
//!
//! # 核心概念
//!
//! - **Block**：prompt 的最小组成单元，带有语义分类（[`BlockKind`]）、优先级、条件、依赖等元数据
//! - **Contributor**：独立的 prompt 内容提供者，如身份、环境、规则、工具指南等
//! - **Composer**：管线编排器，负责收集、去重、拓扑排序、渲染和验证
//! - **Agent Profile Summary**：基于动态 profile catalog 生成的子 Agent 索引块，供 `spawn` 路由使用
//! - **Skill Summary**：基于外部 skill catalog 生成的索引摘要块，供 `Skill` tool 两阶段加载模型使用
//!
//! # 设计原则
//!
//! - 每个 contributor 保持编译隔离，通过 trait 接口组合
//! - prompt 块支持条件渲染（如仅首步、特定工具可用时）
//! - 依赖解析采用波前式拓扑排序，自动检测循环依赖
//! - 内置 skill 资源由 `build.rs` 在编译期打包，避免手写 `include_str!` 清单

pub mod block;
pub mod composer;
pub mod context;
pub mod contribution;
pub mod contributor;
pub mod contributors;
pub mod core_port;
pub mod diagnostics;
pub mod layered_builder;
pub mod prompt_plan;
pub mod template;

pub use block::{
    BlockCondition, BlockContent, BlockKind, BlockSpec, PromptBlock, PromptLayer, RenderTarget,
    ValidationPolicy,
};
pub use composer::{PromptBuildOutput, PromptComposer, PromptComposerOptions, ValidationLevel};
pub use context::{PromptAgentProfileSummary, PromptContext, PromptSkillSummary};
pub use contribution::{PromptContribution, append_unique_tools};
pub use contributor::PromptContributor;
pub use diagnostics::{DiagnosticLevel, PromptDiagnostics};
pub use layered_builder::{
    LayeredBuilderOptions, LayeredPromptBuilder, default_layered_prompt_builder,
};
pub use prompt_plan::PromptPlan;
pub use template::TemplateRenderError;
