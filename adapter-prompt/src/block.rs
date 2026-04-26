//! Prompt 块的定义与元数据。
//!
//! 本模块定义了 prompt 组装的最小单元——**Block**。每个 block 带有语义分类、
//! 优先级、渲染目标、条件、依赖等元数据，由 [`PromptComposer`](crate::composer::PromptComposer)
//! 统一编排。
//!
//! # 设计意图
//!
//! 将 prompt 拆分为独立的 block 而非单一字符串，目的是：
//! - 支持条件渲染（如仅在特定步骤或工具可用时包含）
//! - 支持依赖排序（如 few-shot assistant 依赖 few-shot user）
//! - 支持去重（同一 block id 只渲染一次）
//! - 支持诊断（渲染失败时可精确定位到具体 block）

use std::{borrow::Cow, collections::HashMap};

pub use astrcode_core::policy::SystemPromptLayer as PromptLayer;

use super::template::PromptTemplate;

/// Prompt 块的语义分类。
///
/// 决定 block 在组装后的 system prompt 中的默认优先级顺序。
/// 优先级数值越小，出现位置越靠前。
///
/// # 优先级设计原则
///
/// 身份和环境信息放在最前面（让模型先了解"我是谁"和"我在哪"），
/// 规则和指南居中（核心行为约束），示例和技能摘要靠后（补充上下文）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BlockKind {
    /// Agent 身份定义（AI 是谁）。优先级 100。
    ///
    /// 作为 system prompt 的第一部分，建立模型的基本行为准则和角色定位。
    Identity,
    /// 工作环境信息（工作目录、操作系统、日期、工具列表）。优先级 300。
    ///
    /// 让模型了解当前运行环境，以便生成正确的路径、命令等。
    Environment,
    /// 用户级规则（来自 `~/.astrcode/AGENTS.md`）。优先级 400。
    ///
    /// 用户个人的全局偏好和约束，适用于所有项目。
    UserRules,
    /// 项目级规则（来自 `./AGENTS.md`）。优先级 500。
    ///
    /// 项目特定的开发约定、架构决策和注意事项。
    ProjectRules,
    /// 单个工具的使用指南（摘要 + 详细指南）。优先级 550。
    ToolGuide,
    /// 多工具工作流指南（如"先读后改"）。优先级 560。
    SkillGuide,
    /// 插件或 MCP 注入的 prompt 指令。优先级 580。
    ExtensionInstruction,
    /// 子 Agent 协作决策指南（close-or-keep）。优先级 590。
    CollaborationGuide,
    /// Skill 摘要块（仅工具名列表）。优先级 600。
    Skill,
    /// Few-shot 示例消息对。优先级 700。
    ///
    /// 通过示例对话引导模型行为，通常以 prepend 方式插入到用户/助手消息中。
    FewShotExamples,
}

impl BlockKind {
    pub fn default_priority(self) -> i32 {
        match self {
            Self::Identity => 100,
            Self::Environment => 300,
            Self::UserRules => 400,
            Self::ProjectRules => 500,
            Self::ToolGuide => 550,
            Self::SkillGuide => 560,
            Self::ExtensionInstruction => 580,
            Self::CollaborationGuide => 590,
            Self::Skill => 600,
            Self::FewShotExamples => 700,
        }
    }
}

/// Block 内容的渲染目标。
///
/// 决定 block 渲染后放入 LLM 请求的哪个位置。
/// 大多数 block 渲染到 system prompt，但 few-shot 示例需要插入到对话消息中。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderTarget {
    /// 渲染到 system prompt（默认目标）。
    System,
    /// 插入到用户消息列表的头部。
    PrependUser,
    /// 插入到助手消息列表的头部。
    PrependAssistant,
    /// 追加到用户消息列表的尾部。
    AppendUser,
    /// 追加到助手消息列表的尾部。
    AppendAssistant,
}

/// Block 的验证策略。
///
/// 控制当 block 渲染或验证失败时的行为：是静默跳过、记录警告还是抛出错误。
/// 这允许不同重要程度的 block 采用不同的容错策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ValidationPolicy {
    /// 继承 composer 的全局验证级别设置。
    Inherit,
    /// 跳过验证，即使失败也不报告。
    Skip,
    /// 强制严格验证，失败时抛出错误。
    Strict,
}

/// Block 的渲染条件。
///
/// 用于控制 block 是否在当前上下文中被包含。
/// 常见场景：仅在首步包含 few-shot 示例、仅在特定工具可用时包含对应指南。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum BlockCondition {
    /// 无条件始终包含。
    #[default]
    Always,
    /// 仅在指定步骤索引时包含。
    StepEquals(usize),
    /// 仅在第一步（step_index == 0）时包含。
    FirstStepOnly,
    /// 仅在指定工具可用时包含。
    HasTool(String),
    /// 当指定变量的值匹配时包含。
    VarEquals { key: String, expected: String },
}

/// Block 的附加元数据。
///
/// 用于诊断、调试和来源追踪，不参与渲染逻辑。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BlockMetadata {
    /// 标签列表，如 `source:builtin`、`capability:shell`。
    pub tags: Vec<Cow<'static, str>>,
    /// 分类标识，如 `capabilities`、`skills`、`extensions`。
    pub category: Option<Cow<'static, str>>,
    /// 来源描述（如文件路径），用于诊断信息。
    pub origin: Option<String>,
}

impl BlockMetadata {
    /// 返回规范化后的来源标签值。
    ///
    /// `source:*` 目前仍存放在 tags 中，这里集中做一次解析，
    /// 让上层不需要自己扫描 tag 约定。
    pub fn source_name(&self) -> Option<&str> {
        self.tags.iter().find_map(|tag| tag.strip_prefix("source:"))
    }
}

/// Block 的内容形式。
///
/// 支持纯文本和模板两种形式。模板在渲染时会通过变量解析器填充占位符。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BlockContent {
    /// 纯文本内容，无需渲染。
    Text(String),
    /// 模板内容，包含 `{{variable}}` 占位符，需在渲染时解析。
    Template(PromptTemplate),
}

/// Block 的规格定义（贡献者产出的原始数据）。
///
/// 这是 contributor 向 composer 提交的"原始素材"，包含 block 的所有元数据。
/// composer 会对其进行条件过滤、依赖解析、模板渲染后，转为 [`PromptBlock`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlockSpec {
    pub id: Cow<'static, str>,
    pub kind: BlockKind,
    pub title: Cow<'static, str>,
    pub content: BlockContent,
    pub priority: Option<i32>,
    pub condition: BlockCondition,
    pub dependencies: Vec<Cow<'static, str>>,
    pub validation_policy: ValidationPolicy,
    pub render_target: RenderTarget,
    pub metadata: BlockMetadata,
    pub vars: HashMap<String, String>,
    pub layer: PromptLayer,
}

impl BlockSpec {
    pub fn system_text(
        id: impl Into<Cow<'static, str>>,
        kind: BlockKind,
        title: impl Into<Cow<'static, str>>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            title: title.into(),
            content: BlockContent::Text(content.into()),
            priority: None,
            condition: BlockCondition::Always,
            dependencies: Vec::new(),
            validation_policy: ValidationPolicy::Inherit,
            render_target: RenderTarget::System,
            metadata: BlockMetadata::default(),
            vars: HashMap::new(),
            layer: PromptLayer::Unspecified,
        }
    }

    pub fn system_template(
        id: impl Into<Cow<'static, str>>,
        kind: BlockKind,
        title: impl Into<Cow<'static, str>>,
        template: impl Into<Cow<'static, str>>,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            title: title.into(),
            content: BlockContent::Template(PromptTemplate::new(template)),
            priority: None,
            condition: BlockCondition::Always,
            dependencies: Vec::new(),
            validation_policy: ValidationPolicy::Inherit,
            render_target: RenderTarget::System,
            metadata: BlockMetadata::default(),
            vars: HashMap::new(),
            layer: PromptLayer::Unspecified,
        }
    }

    pub fn message_text(
        id: impl Into<Cow<'static, str>>,
        kind: BlockKind,
        title: impl Into<Cow<'static, str>>,
        content: impl Into<String>,
        render_target: RenderTarget,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            title: title.into(),
            content: BlockContent::Text(content.into()),
            priority: None,
            condition: BlockCondition::Always,
            dependencies: Vec::new(),
            validation_policy: ValidationPolicy::Inherit,
            render_target,
            metadata: BlockMetadata::default(),
            vars: HashMap::new(),
            layer: PromptLayer::Unspecified,
        }
    }

    pub fn with_priority(mut self, priority: i32) -> Self {
        self.priority = Some(priority);
        self
    }

    pub fn with_condition(mut self, condition: BlockCondition) -> Self {
        self.condition = condition;
        self
    }

    pub fn depends_on(mut self, dependency: impl Into<Cow<'static, str>>) -> Self {
        self.dependencies.push(dependency.into());
        self
    }

    pub fn with_tag(mut self, tag: impl Into<Cow<'static, str>>) -> Self {
        self.metadata.tags.push(tag.into());
        self
    }

    pub fn with_category(mut self, category: impl Into<Cow<'static, str>>) -> Self {
        self.metadata.category = Some(category.into());
        self
    }

    pub fn with_origin(mut self, origin: impl Into<String>) -> Self {
        self.metadata.origin = Some(origin.into());
        self
    }

    pub fn with_var(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.vars.insert(key.into(), value.into());
        self
    }

    pub fn with_layer(mut self, layer: PromptLayer) -> Self {
        self.layer = layer;
        self
    }

    pub fn effective_priority(&self) -> i32 {
        self.priority
            .unwrap_or_else(|| self.kind.default_priority())
    }
}

/// 渲染后的 prompt block（已填入最终内容）。
///
/// 由 [`BlockSpec`] 经过条件过滤、模板渲染、验证后生成，
/// 是 [`PromptPlan`](crate::plan::PromptPlan) 中的最终产物。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptBlock {
    pub id: String,
    pub kind: BlockKind,
    pub title: String,
    pub content: String,
    pub priority: i32,
    pub layer: PromptLayer,
    pub metadata: BlockMetadata,
    pub insertion_order: usize,
}

impl PromptBlock {
    pub fn new(
        id: impl Into<String>,
        kind: BlockKind,
        title: impl Into<String>,
        content: impl Into<String>,
        priority: i32,
        metadata: BlockMetadata,
        insertion_order: usize,
    ) -> Self {
        Self {
            id: id.into(),
            kind,
            title: title.into(),
            content: content.into(),
            priority,
            layer: PromptLayer::Unspecified,
            metadata,
            insertion_order,
        }
    }

    pub fn with_layer(mut self, layer: PromptLayer) -> Self {
        self.layer = layer;
        self
    }
}
