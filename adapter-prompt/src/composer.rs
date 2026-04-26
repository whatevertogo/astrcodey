//! Prompt 组装引擎（PromptComposer）。
//!
//! 本模块是整个 prompt 组装管线的核心编排器。
//!
//! # 工作流程
//!
//! 1. **收集阶段**：依次调用每个 [`PromptContributor::contribute`] 收集 [`PromptContribution`]
//! 2. **去重阶段**：过滤重复的 block id，保留首次出现的贡献
//! 3. **条件过滤**：根据 [`BlockCondition`] 排除不满足条件的 block
//! 4. **依赖解析**：采用波前式拓扑排序处理 block 间的依赖关系
//! 5. **渲染阶段**：将模板中的 `{{variable}}` 占位符替换为实际值
//! 6. **验证阶段**：检查渲染结果的有效性（非空标题、非空内容等）
//! 7. **输出阶段**：根据 [`RenderTarget`] 将 block 分配到 system prompt 或对话消息
//!
//! # 缓存策略
//!
//! Contributor 的收集结果会被缓存，缓存键由 `contributor_id + cache_version + fingerprint` 组成。
//! 当上下文未变化且 TTL 未过期时，直接复用缓存结果，避免重复的文件 I/O。
//!
//! # 依赖解析算法
//!
//! 采用波前式拓扑排序（wave-based topological sort）：
//! 每轮迭代处理所有依赖已就绪的候选块，未就绪的推迟到下一轮。
//! 如果一轮迭代中没有任何进展，说明存在循环依赖。
//! 这种方式比标准 Kahn 算法更简单，且能自然地产生诊断信息。

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::{Result, anyhow};
use astrcode_core::{LlmMessage, UserMessageOrigin};
use astrcode_runtime_contract::prompt::PromptCacheHints;

use super::{
    BlockCondition, BlockContent, BlockKind, BlockSpec, PromptBlock, PromptContext,
    PromptContribution, PromptContributor, PromptPlan, RenderTarget, TemplateRenderError,
    ValidationPolicy, append_unique_tools,
    contributors::{
        AgentProfileSummaryContributor, AgentsMdContributor, CapabilityPromptContributor,
        EnvironmentContributor, IdentityContributor, SkillSummaryContributor,
        WorkflowExamplesContributor,
    },
    diagnostics::{DiagnosticLevel, DiagnosticReason, PromptDiagnostic, PromptDiagnostics},
};

/// 验证级别。
///
/// 控制当 block 渲染或验证失败时的行为。
/// 从宽松到严格依次为：关闭 → 警告 → 错误。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValidationLevel {
    /// 关闭验证，失败时静默跳过。
    Off,
    /// 记录警告但继续构建（默认）。
    #[default]
    Warn,
    /// 抛出错误，终止构建。
    Strict,
}

/// PromptComposer 的配置选项。
#[derive(Debug, Clone)]
pub struct PromptComposerOptions {
    /// 是否启用诊断信息收集。
    pub enable_diagnostics: bool,
    /// 验证级别，控制失败时的行为。
    pub validation_level: ValidationLevel,
    /// 贡献者缓存的 TTL（Time-To-Live）。
    /// 设为 0 表示永不过期（依赖指纹检测失效）。
    pub cache_ttl: Duration,
}

impl Default for PromptComposerOptions {
    fn default() -> Self {
        Self {
            enable_diagnostics: true,
            validation_level: ValidationLevel::Warn,
            cache_ttl: Duration::from_secs(0),
        }
    }
}

/// Prompt 组装引擎。
///
/// 负责收集所有 contributor 的贡献，进行去重、条件过滤、依赖解析、
/// 模板渲染和验证，最终产出 [`PromptBuildOutput`]。
///
/// # 使用方式
///
/// ```ignore
/// let composer = PromptComposer::with_defaults();
/// let output = composer.build(&ctx).await?;
/// let system_prompt = output.plan.render_system();
/// ```
///
/// 也可以通过 `with_contributor()` 链式添加自定义贡献者。
pub struct PromptComposer {
    contributors: Vec<Arc<dyn PromptContributor>>,
    options: PromptComposerOptions,
    contributor_cache: Mutex<HashMap<String, ContributorCacheEntry>>,
}

/// Prompt 构建的输出结果。
///
/// 包含组装好的 [`PromptPlan`] 和构建过程中收集的 [`PromptDiagnostics`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptBuildOutput {
    pub plan: PromptPlan,
    pub diagnostics: PromptDiagnostics,
    pub cache_hints: PromptCacheHints,
}

/// 贡献者缓存条目。
///
/// 当 contributor 的指纹未变化且 TTL 未过期时，复用此缓存条目。
#[derive(Debug, Clone)]
struct ContributorCacheEntry {
    fingerprint: String,
    cached_at: Instant,
    contribution: PromptContribution,
}

/// 候选 block（尚未经过条件过滤和渲染）。
///
/// 由 contributor 产出的 [`BlockSpec`] 加上来源信息和插入顺序组成。
#[derive(Debug, Clone)]
struct CandidateBlock {
    spec: BlockSpec,
    contributor_id: &'static str,
    contributor_vars: HashMap<String, String>,
    insertion_order: usize,
}

/// Block 在依赖解析后的最终状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockStatus {
    /// 成功渲染。
    Success,
    /// 因条件不满足被跳过。
    Skipped,
}

impl PromptComposer {
    /// 创建空的 composer，不注册任何贡献者。
    pub fn new(options: PromptComposerOptions) -> Self {
        Self {
            contributors: Vec::new(),
            options,
            contributor_cache: Mutex::new(HashMap::new()),
        }
    }

    /// 使用默认选项和内置贡献者链创建 composer。
    ///
    /// 内置贡献者包括：Identity、Environment、AgentsMd、CapabilityPrompt、
    /// AgentProfileSummary、SkillSummary、WorkflowExamples。
    pub fn with_defaults() -> Self {
        Self::with_options(PromptComposerOptions::default())
    }

    /// 使用指定选项和内置贡献者链创建 composer。
    pub fn with_options(options: PromptComposerOptions) -> Self {
        Self::new(options)
            .with_contributor(Arc::new(IdentityContributor))
            .with_contributor(Arc::new(EnvironmentContributor))
            .with_contributor(Arc::new(AgentsMdContributor))
            .with_contributor(Arc::new(CapabilityPromptContributor))
            .with_contributor(Arc::new(AgentProfileSummaryContributor))
            .with_contributor(Arc::new(SkillSummaryContributor))
            .with_contributor(Arc::new(WorkflowExamplesContributor))
    }

    /// Appends a contributor to the chain.
    ///
    /// Not named `add` to avoid confusion with `std::ops::Add`.
    pub fn with_contributor(mut self, contributor: Arc<dyn PromptContributor>) -> Self {
        self.contributors.push(contributor);
        self
    }

    /// 执行完整的 prompt 组装管线。
    ///
    /// 返回 [`PromptBuildOutput`] 包含组装好的计划和诊断信息。
    ///
    /// # 管线步骤
    ///
    /// 1. 收集所有 contributor 的贡献（带缓存）
    /// 2. 合并额外工具定义
    /// 3. 去重 block id
    /// 4. 条件过滤 + 波前式拓扑排序
    /// 5. 模板渲染 + 验证
    /// 6. 按 render_target 分配到 system/append/prepend
    pub async fn build(&self, ctx: &PromptContext) -> Result<PromptBuildOutput> {
        let mut diagnostics = PromptDiagnostics::default();
        let mut plan = PromptPlan::default();
        let mut candidates = Vec::new();
        let mut insertion_order = 0usize;

        for contributor in &self.contributors {
            let contribution = self
                .collect_contribution(contributor.as_ref(), ctx, &mut diagnostics)
                .await?;

            append_unique_tools(&mut plan.extra_tools, contribution.extra_tools.clone());

            for spec in contribution.blocks {
                candidates.push(CandidateBlock {
                    spec,
                    contributor_id: contributor.contributor_id(),
                    contributor_vars: contribution.contributor_vars.clone(),
                    insertion_order,
                });
                insertion_order += 1;
            }
        }

        let candidates = self.filter_duplicate_block_ids(candidates, &mut diagnostics)?;
        self.resolve_candidates(candidates, ctx, &mut plan, &mut diagnostics)?;

        Ok(PromptBuildOutput {
            plan,
            diagnostics,
            cache_hints: PromptCacheHints::default(),
        })
    }

    /// 收集单个 contributor 的贡献，带缓存逻辑。
    ///
    /// 先检查缓存（指纹匹配 + TTL 未过期），命中则直接返回；
    /// 否则调用 `contribute()` 并存储结果到缓存。
    async fn collect_contribution(
        &self,
        contributor: &dyn PromptContributor,
        ctx: &PromptContext,
        diagnostics: &mut PromptDiagnostics,
    ) -> Result<PromptContribution> {
        let fingerprint = format!(
            "{}:{}:{}",
            contributor.contributor_id(),
            contributor.cache_version(),
            contributor.cache_fingerprint(ctx)
        );

        if let Some(hit) = self.lookup_cache(contributor.contributor_id(), &fingerprint) {
            diagnostics
                .push_cache_reuse_hit(contributor.contributor_id(), Some(fingerprint.clone()));
            return Ok(hit);
        }

        diagnostics.push_cache_reuse_miss(
            contributor.contributor_id(),
            Some(fingerprint.clone()),
            None,
        );

        let contribution = contributor.contribute(ctx).await;
        self.store_cache(
            contributor.contributor_id(),
            fingerprint,
            contribution.clone(),
        );
        Ok(contribution)
    }

    fn lookup_cache(&self, contributor_id: &str, fingerprint: &str) -> Option<PromptContribution> {
        let cache = self
            .contributor_cache
            .lock()
            .expect("contributor cache lock should work");
        let entry = cache.get(contributor_id)?;
        let ttl_valid =
            self.options.cache_ttl.is_zero() || entry.cached_at.elapsed() <= self.options.cache_ttl;

        if entry.fingerprint == fingerprint && ttl_valid {
            Some(entry.contribution.clone())
        } else {
            None
        }
    }

    fn store_cache(
        &self,
        contributor_id: &str,
        fingerprint: String,
        contribution: PromptContribution,
    ) {
        let mut cache = self
            .contributor_cache
            .lock()
            .expect("contributor cache lock should work");
        cache.insert(
            contributor_id.to_string(),
            ContributorCacheEntry {
                fingerprint,
                cached_at: Instant::now(),
                contribution,
            },
        );
    }

    fn filter_duplicate_block_ids(
        &self,
        candidates: Vec<CandidateBlock>,
        diagnostics: &mut PromptDiagnostics,
    ) -> Result<Vec<CandidateBlock>> {
        let mut seen = HashSet::new();
        let mut filtered = Vec::new();

        for candidate in candidates {
            let block_id = candidate.spec.id.to_string();
            if seen.insert(block_id.clone()) {
                filtered.push(candidate);
                continue;
            }

            self.handle_failure(
                diagnostics,
                Some(block_id),
                Some(candidate.contributor_id.to_string()),
                DiagnosticReason::ValidationFailed {
                    message: "duplicate block id".to_string(),
                },
                Some("Ensure each block id is unique within the composed prompt.".to_string()),
                candidate.spec.validation_policy,
            )?;
        }

        Ok(filtered)
    }

    fn resolve_candidates(
        &self,
        candidates: Vec<CandidateBlock>,
        ctx: &PromptContext,
        plan: &mut PromptPlan,
        diagnostics: &mut PromptDiagnostics,
    ) -> Result<()> {
        // 波前式拓扑排序（wave-based topological sort）：
        // 每轮迭代处理所有依赖已就绪的候选块，未就绪的推迟到下一轮。
        // 如果一轮迭代中没有任何进展（progressed == false），说明存在循环依赖。
        // 这种方式比标准 Kahn 算法更简单，且能自然地产生诊断信息。
        let known_ids = candidates
            .iter()
            .map(|candidate| candidate.spec.id.to_string())
            .collect::<HashSet<_>>();
        let mut statuses = HashMap::<String, BlockStatus>::new();
        let mut pending = Vec::new();

        for candidate in candidates {
            if self.condition_matches(&candidate.spec.condition, ctx, &candidate.contributor_vars) {
                pending.push(candidate);
            } else {
                statuses.insert(candidate.spec.id.to_string(), BlockStatus::Skipped);
            }
        }

        while !pending.is_empty() {
            let mut next_pending = Vec::new();
            let mut progressed = false;

            for candidate in pending {
                match self.dependencies_ready(&candidate, &known_ids, &statuses) {
                    DependencyState::Ready => {
                        progressed = true;
                        match self.render_candidate(&candidate, ctx, diagnostics)? {
                            Some(rendered) => {
                                self.push_rendered(plan, rendered, &candidate);
                                statuses
                                    .insert(candidate.spec.id.to_string(), BlockStatus::Success);
                            },
                            None => {
                                statuses
                                    .insert(candidate.spec.id.to_string(), BlockStatus::Skipped);
                            },
                        }
                    },
                    DependencyState::Blocked(dependency_id) => {
                        progressed = true;
                        statuses.insert(candidate.spec.id.to_string(), BlockStatus::Skipped);
                        self.push_diagnostic(
                            diagnostics,
                            DiagnosticLevel::Warning,
                            Some(candidate.spec.id.to_string()),
                            Some(candidate.contributor_id.to_string()),
                            DiagnosticReason::MissingDependency { dependency_id },
                            Some(
                                "Check whether the dependency exists and was not skipped or \
                                 invalidated."
                                    .to_string(),
                            ),
                        );
                    },
                    DependencyState::Pending => next_pending.push(candidate),
                }
            }

            if !progressed {
                for candidate in next_pending {
                    let dependency_id = candidate
                        .spec
                        .dependencies
                        .first()
                        .map(|dependency| dependency.to_string())
                        .unwrap_or_else(|| "<cycle>".to_string());
                    statuses.insert(candidate.spec.id.to_string(), BlockStatus::Skipped);
                    self.push_diagnostic(
                        diagnostics,
                        DiagnosticLevel::Warning,
                        Some(candidate.spec.id.to_string()),
                        Some(candidate.contributor_id.to_string()),
                        DiagnosticReason::MissingDependency { dependency_id },
                        Some(
                            "Dependencies must resolve successfully before the block can render."
                                .to_string(),
                        ),
                    );
                }
                break;
            }

            pending = next_pending;
        }

        Ok(())
    }

    fn condition_matches(
        &self,
        condition: &BlockCondition,
        ctx: &PromptContext,
        contributor_vars: &HashMap<String, String>,
    ) -> bool {
        match condition {
            BlockCondition::Always => true,
            BlockCondition::StepEquals(step) => ctx.step_index == *step,
            BlockCondition::FirstStepOnly => ctx.step_index == 0,
            BlockCondition::HasTool(tool) => ctx.tool_names.iter().any(|name| name == tool),
            BlockCondition::VarEquals { key, expected } => {
                contributor_vars
                    .get(key)
                    .cloned()
                    .or_else(|| ctx.resolve_global_var(key))
                    .or_else(|| ctx.resolve_builtin_var(key))
                    .as_deref()
                    == Some(expected.as_str())
            },
        }
    }

    fn dependencies_ready(
        &self,
        candidate: &CandidateBlock,
        known_ids: &HashSet<String>,
        statuses: &HashMap<String, BlockStatus>,
    ) -> DependencyState {
        for dependency in &candidate.spec.dependencies {
            let dependency_id = dependency.to_string();
            if !known_ids.contains(&dependency_id) {
                return DependencyState::Blocked(dependency_id);
            }

            match statuses.get(&dependency_id) {
                Some(BlockStatus::Success) => {},
                Some(BlockStatus::Skipped) => return DependencyState::Blocked(dependency_id),
                None => return DependencyState::Pending,
            }
        }

        DependencyState::Ready
    }

    fn render_candidate(
        &self,
        candidate: &CandidateBlock,
        ctx: &PromptContext,
        diagnostics: &mut PromptDiagnostics,
    ) -> Result<Option<String>> {
        let rendered = match &candidate.spec.content {
            BlockContent::Text(content) => content.clone(),
            BlockContent::Template(template) => match template.render(|key| {
                candidate
                    .spec
                    .vars
                    .get(key)
                    .cloned()
                    .or_else(|| candidate.contributor_vars.get(key).cloned())
                    .or_else(|| ctx.resolve_global_var(key))
                    .or_else(|| ctx.resolve_builtin_var(key))
            }) {
                Ok(content) => content,
                Err(TemplateRenderError::MissingVariable(variable)) => {
                    self.handle_failure(
                        diagnostics,
                        Some(candidate.spec.id.to_string()),
                        Some(candidate.contributor_id.to_string()),
                        DiagnosticReason::TemplateVariableMissing { variable },
                        Some(
                            "Provide the variable in block vars, contributor vars, PromptContext \
                             vars, or builtins."
                                .to_string(),
                        ),
                        candidate.spec.validation_policy,
                    )?;
                    return Ok(None);
                },
                Err(error) => {
                    self.handle_failure(
                        diagnostics,
                        Some(candidate.spec.id.to_string()),
                        Some(candidate.contributor_id.to_string()),
                        DiagnosticReason::RenderFailed {
                            message: error.to_string(),
                        },
                        None,
                        candidate.spec.validation_policy,
                    )?;
                    return Ok(None);
                },
            },
        };

        if !self.validate_render_target(&candidate.spec) {
            self.handle_failure(
                diagnostics,
                Some(candidate.spec.id.to_string()),
                Some(candidate.contributor_id.to_string()),
                DiagnosticReason::ValidationFailed {
                    message: "few-shot blocks must render as prepend/append messages".to_string(),
                },
                None,
                candidate.spec.validation_policy,
            )?;
            return Ok(None);
        }

        if candidate.spec.title.trim().is_empty() {
            self.handle_failure(
                diagnostics,
                Some(candidate.spec.id.to_string()),
                Some(candidate.contributor_id.to_string()),
                DiagnosticReason::ValidationFailed {
                    message: "block title must not be empty".to_string(),
                },
                None,
                candidate.spec.validation_policy,
            )?;
            return Ok(None);
        }

        if rendered.trim().is_empty() {
            self.handle_failure(
                diagnostics,
                Some(candidate.spec.id.to_string()),
                Some(candidate.contributor_id.to_string()),
                DiagnosticReason::ValidationFailed {
                    message: "block content must not be empty".to_string(),
                },
                None,
                candidate.spec.validation_policy,
            )?;
            return Ok(None);
        }

        Ok(Some(rendered))
    }

    fn validate_render_target(&self, spec: &BlockSpec) -> bool {
        !matches!(spec.kind, BlockKind::FewShotExamples)
            || !matches!(spec.render_target, RenderTarget::System)
    }

    fn push_rendered(&self, plan: &mut PromptPlan, rendered: String, candidate: &CandidateBlock) {
        match candidate.spec.render_target {
            RenderTarget::System => plan.system_blocks.push(
                PromptBlock::new(
                    candidate.spec.id.to_string(),
                    candidate.spec.kind,
                    candidate.spec.title.to_string(),
                    rendered,
                    candidate.spec.effective_priority(),
                    candidate.spec.metadata.clone(),
                    candidate.insertion_order,
                )
                .with_layer(candidate.spec.layer),
            ),
            RenderTarget::PrependUser => plan.prepend_messages.push(LlmMessage::User {
                content: rendered,
                origin: UserMessageOrigin::User,
            }),
            RenderTarget::PrependAssistant => plan.prepend_messages.push(LlmMessage::Assistant {
                content: rendered,
                tool_calls: vec![],
                reasoning: None,
            }),
            RenderTarget::AppendUser => plan.append_messages.push(LlmMessage::User {
                content: rendered,
                origin: UserMessageOrigin::User,
            }),
            RenderTarget::AppendAssistant => plan.append_messages.push(LlmMessage::Assistant {
                content: rendered,
                tool_calls: vec![],
                reasoning: None,
            }),
        }
    }

    fn handle_failure(
        &self,
        diagnostics: &mut PromptDiagnostics,
        block_id: Option<String>,
        contributor_id: Option<String>,
        reason: DiagnosticReason,
        suggestion: Option<String>,
        validation_policy: ValidationPolicy,
    ) -> Result<()> {
        match self.effective_validation_level(validation_policy) {
            ValidationLevel::Off => Ok(()),
            ValidationLevel::Warn => {
                self.push_diagnostic(
                    diagnostics,
                    DiagnosticLevel::Warning,
                    block_id,
                    contributor_id,
                    reason,
                    suggestion,
                );
                Ok(())
            },
            ValidationLevel::Strict => Err(anyhow!("prompt block validation failed: {:?}", reason)),
        }
    }

    fn effective_validation_level(&self, validation_policy: ValidationPolicy) -> ValidationLevel {
        match validation_policy {
            ValidationPolicy::Inherit => self.options.validation_level,
            ValidationPolicy::Skip => ValidationLevel::Off,
            ValidationPolicy::Strict => ValidationLevel::Strict,
        }
    }

    fn push_diagnostic(
        &self,
        diagnostics: &mut PromptDiagnostics,
        level: DiagnosticLevel,
        block_id: Option<String>,
        contributor_id: Option<String>,
        reason: DiagnosticReason,
        suggestion: Option<String>,
    ) {
        if !self.options.enable_diagnostics {
            return;
        }

        diagnostics.push(PromptDiagnostic {
            level,
            block_id,
            contributor_id,
            reason,
            suggestion,
            timestamp: chrono::Utc::now(),
        });
    }
}

enum DependencyState {
    Ready,
    Blocked(String),
    Pending,
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use astrcode_core::test_support::TestEnvGuard;
    use async_trait::async_trait;

    use super::*;

    fn test_context(working_dir: String) -> PromptContext {
        PromptContext {
            working_dir,
            tool_names: vec!["shell".to_string()],
            capability_specs: Vec::new(),
            system_prompt_instructions: Vec::new(),
            agent_profiles: Vec::new(),
            skills: Vec::new(),
            step_index: 0,
            turn_index: 0,
            vars: HashMap::new(),
        }
    }

    #[tokio::test]
    async fn with_defaults_build_includes_identity_block() {
        let _guard = TestEnvGuard::new();
        let project = tempfile::tempdir().expect("tempdir should be created");
        let composer = PromptComposer::with_defaults();

        let output = composer
            .build(&test_context(project.path().to_string_lossy().into_owned()))
            .await
            .expect("build should succeed");

        assert!(
            output
                .plan
                .system_blocks
                .iter()
                .any(|block| block.kind == BlockKind::Identity)
        );
    }

    #[tokio::test]
    async fn template_resolution_prefers_block_then_contributor_then_context_then_builtin() {
        let _guard = TestEnvGuard::new();
        struct TemplateContributor;

        #[async_trait]
        impl PromptContributor for TemplateContributor {
            fn contributor_id(&self) -> &'static str {
                "template"
            }

            async fn contribute(&self, _ctx: &PromptContext) -> PromptContribution {
                let mut contribution = PromptContribution {
                    blocks: vec![
                        BlockSpec::system_template(
                            "scoped",
                            BlockKind::Skill,
                            "Skill",
                            "{{name}}|{{project.name}}|{{project.working_dir}}|{{env.os}}",
                        )
                        .with_var("name", "block"),
                    ],
                    ..PromptContribution::default()
                };
                contribution
                    .contributor_vars
                    .insert("project.name".to_string(), "contributor".to_string());
                contribution
            }
        }

        let composer = PromptComposer::with_options(PromptComposerOptions {
            validation_level: ValidationLevel::Strict,
            ..PromptComposerOptions::default()
        })
        .with_contributor(Arc::new(TemplateContributor));
        let mut ctx = test_context("/workspace/demo".to_string());
        ctx.vars
            .insert("project.name".to_string(), "context".to_string());

        let output = composer.build(&ctx).await.expect("build should succeed");
        let block = output
            .plan
            .system_blocks
            .iter()
            .find(|block| block.id == "scoped")
            .expect("scoped block should exist");

        assert!(
            block
                .content
                .starts_with("block|contributor|/workspace/demo|")
        );
    }

    #[tokio::test]
    async fn strict_validation_bubbles_up_error() {
        let _guard = TestEnvGuard::new();
        struct InvalidContributor;

        #[async_trait]
        impl PromptContributor for InvalidContributor {
            fn contributor_id(&self) -> &'static str {
                "invalid"
            }

            async fn contribute(&self, _ctx: &PromptContext) -> PromptContribution {
                PromptContribution {
                    blocks: vec![BlockSpec::message_text(
                        "few-shot",
                        BlockKind::FewShotExamples,
                        "Few Shot",
                        "bad",
                        RenderTarget::System,
                    )],
                    ..PromptContribution::default()
                }
            }
        }

        let composer = PromptComposer::with_options(PromptComposerOptions {
            validation_level: ValidationLevel::Strict,
            ..PromptComposerOptions::default()
        })
        .with_contributor(Arc::new(InvalidContributor));

        let err = composer
            .build(&test_context("/workspace/demo".to_string()))
            .await
            .expect_err("strict validation should fail");
        assert!(err.to_string().contains("prompt block validation failed"));
    }
}
