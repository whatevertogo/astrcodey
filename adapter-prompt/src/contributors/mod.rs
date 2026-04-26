//! 内置 prompt 贡献者实现。
//!
//! 每个 contributor 负责一个特定领域的 prompt 内容生成：
//! - [`AgentProfileSummaryContributor`]：子 Agent profile 动态索引
//! - [`IdentityContributor`]：AI 身份定义
//! - [`EnvironmentContributor`]：工作环境信息
//! - [`AgentsMdContributor`]：用户和项目级 AGENTS.md 规则
//! - [`CapabilityPromptContributor`]：工具使用指南
//! - [`ResponseStyleContributor`]：用户可见输出风格与收尾格式约束
//! - [`SkillSummaryContributor`]：Skill 索引摘要
//! - [`SystemPromptInstructionContributor`]：运行时注入的 system prompt 分层指令
//! - [`WorkflowExamplesContributor`]：Few-shot 示例对话

pub mod agent_profile_summary;
pub mod agents_md;
pub mod capability_prompt;
pub mod environment;
pub mod identity;
pub mod response_style;
pub mod shared;
pub mod skill_summary;
pub mod system_prompt_instruction;
pub mod workflow_examples;

pub use agent_profile_summary::AgentProfileSummaryContributor;
pub use agents_md::AgentsMdContributor;
pub use capability_prompt::CapabilityPromptContributor;
pub use environment::EnvironmentContributor;
pub use identity::{IdentityContributor, load_identity_md, user_identity_md_path};
pub use response_style::ResponseStyleContributor;
pub use shared::{cache_marker_for_path, user_astrcode_file_path};
pub use skill_summary::SkillSummaryContributor;
pub use system_prompt_instruction::SystemPromptInstructionContributor;
pub use workflow_examples::WorkflowExamplesContributor;
