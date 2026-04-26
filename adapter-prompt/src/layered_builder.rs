//! 分层 Prompt 构建器（Layered Prompt Builder）。
//!
//! 采用“按层独立 build，再合并最终 plan”的方式，把稳定前缀明确沉淀到
//! `PromptPlan.system_blocks` 的层级元数据中，供 Prompt caching / stable prefix 优化使用。

use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use anyhow::Result;
use astrcode_runtime_contract::prompt::{PromptCacheHints, PromptLayerFingerprints};

use super::{
    PromptBuildOutput, PromptComposer, PromptComposerOptions, PromptContext, PromptContributor,
    PromptDiagnostics, PromptLayer, PromptPlan, ValidationLevel,
};

/// 分层 Prompt 构建器。
///
/// 采用四层架构：稳定层 → 半稳定层 → 继承层 → 动态层。
/// 每层单独执行完整的 `PromptComposer` 管线，再按层级合并结果。
#[derive(Clone)]
pub struct LayeredPromptBuilder {
    stable_contributors: Vec<Arc<dyn PromptContributor>>,
    semi_stable_contributors: Vec<Arc<dyn PromptContributor>>,
    inherited_contributors: Vec<Arc<dyn PromptContributor>>,
    dynamic_contributors: Vec<Arc<dyn PromptContributor>>,
    cache: Arc<Mutex<LayerCache>>,
    options: LayeredBuilderOptions,
}

impl Default for LayeredPromptBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// 分层构建器的配置选项。
#[derive(Debug, Clone)]
pub struct LayeredBuilderOptions {
    /// 是否启用诊断信息收集。
    pub enable_diagnostics: bool,
    /// 稳定层缓存 TTL（默认永不过期，因为 stable 层几乎不变）。
    pub stable_cache_ttl: Duration,
    /// 半稳定层缓存 TTL（默认 5 分钟）。
    pub semi_stable_cache_ttl: Duration,
    /// 继承层缓存 TTL（默认 5 分钟）。
    pub inherited_cache_ttl: Duration,
    /// 渲染/验证失败时的处理级别。
    pub validation_level: ValidationLevel,
}

impl Default for LayeredBuilderOptions {
    fn default() -> Self {
        Self {
            enable_diagnostics: true,
            stable_cache_ttl: Duration::ZERO,
            semi_stable_cache_ttl: Duration::from_secs(300),
            inherited_cache_ttl: Duration::from_secs(300),
            validation_level: ValidationLevel::Warn,
        }
    }
}

#[derive(Debug, Clone)]
struct LayerCacheEntry {
    fingerprint: String,
    cached_at: Instant,
    output: PromptBuildOutput,
}

#[derive(Debug, Default)]
struct LayerCache {
    stable: HashMap<String, LayerCacheEntry>,
    semi_stable: HashMap<String, LayerCacheEntry>,
    inherited: HashMap<String, LayerCacheEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CacheLookupResult {
    Hit(Box<PromptBuildOutput>),
    Miss { invalidation_reason: String },
}

impl LayeredPromptBuilder {
    pub fn new() -> Self {
        Self::with_options(LayeredBuilderOptions::default())
    }

    pub fn with_options(options: LayeredBuilderOptions) -> Self {
        Self::with_cache(options, Arc::new(Mutex::new(LayerCache::default())))
    }

    fn with_cache(options: LayeredBuilderOptions, cache: Arc<Mutex<LayerCache>>) -> Self {
        Self {
            stable_contributors: Vec::new(),
            semi_stable_contributors: Vec::new(),
            inherited_contributors: Vec::new(),
            dynamic_contributors: Vec::new(),
            cache,
            options,
        }
    }

    pub fn with_stable_layer(mut self, contributors: Vec<Arc<dyn PromptContributor>>) -> Self {
        self.stable_contributors = contributors;
        self
    }

    pub fn with_semi_stable_layer(mut self, contributors: Vec<Arc<dyn PromptContributor>>) -> Self {
        self.semi_stable_contributors = contributors;
        self
    }

    pub fn with_inherited_layer(mut self, contributors: Vec<Arc<dyn PromptContributor>>) -> Self {
        self.inherited_contributors = contributors;
        self
    }

    pub fn with_dynamic_layer(mut self, contributors: Vec<Arc<dyn PromptContributor>>) -> Self {
        self.dynamic_contributors = contributors;
        self
    }

    /// 执行分层 prompt 构建。
    ///
    /// 每层都会运行完整的 composer 流程，因此不会再丢失模板渲染、
    /// 条件过滤、依赖解析和诊断。
    pub async fn build(&self, ctx: &PromptContext) -> Result<PromptBuildOutput> {
        let mut diagnostics = PromptDiagnostics::default();
        let mut plan = PromptPlan::default();
        let mut cache_hints = PromptCacheHints::default();

        for (layer_type, contributors) in [
            (LayerType::Stable, &self.stable_contributors),
            (LayerType::SemiStable, &self.semi_stable_contributors),
            (LayerType::Inherited, &self.inherited_contributors),
            (LayerType::Dynamic, &self.dynamic_contributors),
        ] {
            let output = self.build_layer(contributors, ctx, layer_type).await?;
            merge_layer_cache_hints(&mut cache_hints, layer_type, &output.cache_hints);
            diagnostics.items.extend(output.diagnostics.items);
            plan.extend_with_layer(output.plan, layer_type.prompt_layer());
        }

        Ok(PromptBuildOutput {
            plan,
            diagnostics,
            cache_hints,
        })
    }

    async fn build_layer(
        &self,
        contributors: &[Arc<dyn PromptContributor>],
        ctx: &PromptContext,
        layer_type: LayerType,
    ) -> Result<PromptBuildOutput> {
        if contributors.is_empty() {
            return Ok(PromptBuildOutput {
                plan: PromptPlan::default(),
                diagnostics: PromptDiagnostics::default(),
                cache_hints: PromptCacheHints::default(),
            });
        }

        if layer_type == LayerType::Dynamic {
            return self.render_layer(contributors, ctx).await;
        }

        let mut combined = PromptBuildOutput {
            plan: PromptPlan::default(),
            diagnostics: PromptDiagnostics::default(),
            cache_hints: PromptCacheHints::default(),
        };
        let mut layer_unchanged = layer_type != LayerType::Dynamic;

        for contributor in contributors {
            let contributor_id = contributor.contributor_id();
            let cache_key = format!("{}:{contributor_id}", layer_type.cache_namespace());
            let fingerprint = compute_layer_fingerprint(&[Arc::clone(contributor)], ctx);

            match self.lookup_cache(layer_type, &cache_key, &fingerprint) {
                CacheLookupResult::Hit(output) => {
                    combined
                        .diagnostics
                        .push_cache_reuse_hit(cache_key.clone(), Some(fingerprint.clone()));
                    combined
                        .diagnostics
                        .items
                        .extend(output.diagnostics.items.clone());
                    combined
                        .plan
                        .extend_with_layer(output.plan.clone(), layer_type.prompt_layer());
                },
                CacheLookupResult::Miss {
                    invalidation_reason,
                } => {
                    layer_unchanged = false;
                    combined.diagnostics.push_cache_reuse_miss(
                        cache_key.clone(),
                        Some(fingerprint.clone()),
                        Some(invalidation_reason),
                    );
                    let output = self.render_layer(&[Arc::clone(contributor)], ctx).await?;
                    combined
                        .diagnostics
                        .items
                        .extend(output.diagnostics.items.clone());
                    combined
                        .plan
                        .extend_with_layer(output.plan.clone(), layer_type.prompt_layer());
                    self.store_cache(layer_type, cache_key, fingerprint, output);
                },
            }
        }

        if layer_unchanged {
            combined
                .cache_hints
                .unchanged_layers
                .push(layer_type.prompt_layer());
        }
        set_layer_fingerprint(
            &mut combined.cache_hints.layer_fingerprints,
            layer_type,
            fingerprint_rendered_layer(layer_type, &combined.plan),
        );

        Ok(combined)
    }

    async fn render_layer(
        &self,
        contributors: &[Arc<dyn PromptContributor>],
        ctx: &PromptContext,
    ) -> Result<PromptBuildOutput> {
        let mut composer = PromptComposer::new(PromptComposerOptions {
            enable_diagnostics: self.options.enable_diagnostics,
            validation_level: self.options.validation_level,
            // 分层 build 会重建临时 composer，因此这里不再依赖 contributor 级 TTL；
            // 由 `LayeredPromptBuilder` 自己承接跨 step 的层缓存。
            cache_ttl: Duration::ZERO,
        });
        for contributor in contributors {
            composer = composer.with_contributor(Arc::clone(contributor));
        }
        composer.build(ctx).await
    }

    fn lookup_cache(
        &self,
        layer_type: LayerType,
        cache_key: &str,
        fingerprint: &str,
    ) -> CacheLookupResult {
        let cache = self
            .cache
            .lock()
            .expect("layer cache lock should not be poisoned");
        let entry = layer_type.cache_entries(&cache).get(cache_key);
        let Some(entry) = entry else {
            return CacheLookupResult::Miss {
                invalidation_reason: "cold_start".to_string(),
            };
        };

        if entry.fingerprint == fingerprint && !is_cache_expired(entry, &self.options, layer_type) {
            CacheLookupResult::Hit(Box::new(entry.output.clone()))
        } else if is_cache_expired(entry, &self.options, layer_type) {
            CacheLookupResult::Miss {
                invalidation_reason: "ttl_expired".to_string(),
            }
        } else {
            CacheLookupResult::Miss {
                invalidation_reason: "fingerprint_changed".to_string(),
            }
        }
    }

    fn store_cache(
        &self,
        layer_type: LayerType,
        cache_key: String,
        fingerprint: String,
        output: PromptBuildOutput,
    ) {
        let mut cache = self
            .cache
            .lock()
            .expect("layer cache lock should not be poisoned");
        let entry = LayerCacheEntry {
            fingerprint,
            cached_at: Instant::now(),
            output,
        };

        match layer_type {
            LayerType::Stable => {
                cache.stable.insert(cache_key, entry);
            },
            LayerType::SemiStable => {
                cache.semi_stable.insert(cache_key, entry);
            },
            LayerType::Inherited => {
                cache.inherited.insert(cache_key, entry);
            },
            LayerType::Dynamic => {},
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LayerType {
    Stable,
    SemiStable,
    Inherited,
    Dynamic,
}

impl LayerType {
    fn cache_entries<'a>(&self, cache: &'a LayerCache) -> &'a HashMap<String, LayerCacheEntry> {
        match self {
            Self::Stable => &cache.stable,
            Self::SemiStable => &cache.semi_stable,
            Self::Inherited => &cache.inherited,
            Self::Dynamic => unreachable!("dynamic layer never reads cache entries"),
        }
    }

    fn cache_namespace(self) -> &'static str {
        match self {
            Self::Stable => "stable",
            Self::SemiStable => "semi-stable",
            Self::Inherited => "inherited",
            Self::Dynamic => "dynamic",
        }
    }

    fn prompt_layer(self) -> PromptLayer {
        match self {
            Self::Stable => PromptLayer::Stable,
            Self::SemiStable => PromptLayer::SemiStable,
            Self::Inherited => PromptLayer::Inherited,
            Self::Dynamic => PromptLayer::Dynamic,
        }
    }
}

pub fn default_layered_prompt_builder() -> LayeredPromptBuilder {
    LayeredPromptBuilder::new()
        .with_stable_layer(vec![
            Arc::new(crate::contributors::IdentityContributor),
            Arc::new(crate::contributors::EnvironmentContributor),
            Arc::new(crate::contributors::ResponseStyleContributor),
        ])
        .with_semi_stable_layer(vec![
            Arc::new(crate::contributors::AgentsMdContributor),
            Arc::new(crate::contributors::CapabilityPromptContributor),
            Arc::new(crate::contributors::AgentProfileSummaryContributor),
            Arc::new(crate::contributors::SkillSummaryContributor),
            Arc::new(crate::contributors::SystemPromptInstructionContributor),
        ])
        .with_inherited_layer(Vec::new())
        .with_dynamic_layer(vec![Arc::new(
            crate::contributors::WorkflowExamplesContributor,
        )])
}

fn compute_layer_fingerprint(
    contributors: &[Arc<dyn PromptContributor>],
    ctx: &PromptContext,
) -> String {
    contributors
        .iter()
        .map(|contributor| {
            format!(
                "{}:{}:{}",
                contributor.contributor_id(),
                contributor.cache_version(),
                contributor.cache_fingerprint(ctx)
            )
        })
        .collect::<Vec<_>>()
        .join("|")
}

fn merge_layer_cache_hints(
    target: &mut PromptCacheHints,
    layer_type: LayerType,
    source: &PromptCacheHints,
) {
    set_layer_fingerprint(
        &mut target.layer_fingerprints,
        layer_type,
        layer_fingerprint(source, layer_type).cloned(),
    );
    if source
        .unchanged_layers
        .iter()
        .any(|layer| *layer == layer_type.prompt_layer())
    {
        target.unchanged_layers.push(layer_type.prompt_layer());
    }
}

fn set_layer_fingerprint(
    fingerprints: &mut PromptLayerFingerprints,
    layer_type: LayerType,
    fingerprint: Option<String>,
) {
    match layer_type {
        LayerType::Stable => fingerprints.stable = fingerprint,
        LayerType::SemiStable => fingerprints.semi_stable = fingerprint,
        LayerType::Inherited => fingerprints.inherited = fingerprint,
        LayerType::Dynamic => fingerprints.dynamic = fingerprint,
    }
}

fn layer_fingerprint(hints: &PromptCacheHints, layer_type: LayerType) -> Option<&String> {
    match layer_type {
        LayerType::Stable => hints.layer_fingerprints.stable.as_ref(),
        LayerType::SemiStable => hints.layer_fingerprints.semi_stable.as_ref(),
        LayerType::Inherited => hints.layer_fingerprints.inherited.as_ref(),
        LayerType::Dynamic => hints.layer_fingerprints.dynamic.as_ref(),
    }
}

fn fingerprint_rendered_layer(layer_type: LayerType, plan: &PromptPlan) -> Option<String> {
    let mut hasher = DefaultHasher::new();
    let mut matched = false;
    for block in plan.ordered_system_blocks() {
        if block.layer != layer_type.prompt_layer() {
            continue;
        }
        matched = true;
        block.id.hash(&mut hasher);
        block.title.hash(&mut hasher);
        block.content.hash(&mut hasher);
    }
    matched.then(|| format!("{:x}", hasher.finish()))
}

fn is_cache_expired(
    entry: &LayerCacheEntry,
    options: &LayeredBuilderOptions,
    layer_type: LayerType,
) -> bool {
    let ttl = match layer_type {
        LayerType::Stable => options.stable_cache_ttl,
        LayerType::SemiStable => options.semi_stable_cache_ttl,
        LayerType::Inherited => options.inherited_cache_ttl,
        LayerType::Dynamic => Duration::ZERO,
    };

    if ttl.is_zero() {
        return false;
    }

    entry.cached_at.elapsed() > ttl
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, sync::Arc};

    use async_trait::async_trait;

    use super::*;
    use crate::{BlockKind, BlockSpec, PromptContribution};

    struct StaticContributor {
        id: &'static str,
        block_id: &'static str,
        title: &'static str,
        content: &'static str,
    }

    #[async_trait]
    impl PromptContributor for StaticContributor {
        fn contributor_id(&self) -> &'static str {
            self.id
        }

        async fn contribute(&self, _ctx: &PromptContext) -> PromptContribution {
            PromptContribution {
                blocks: vec![BlockSpec::system_text(
                    self.block_id,
                    BlockKind::ExtensionInstruction,
                    self.title,
                    self.content,
                )],
                ..PromptContribution::default()
            }
        }
    }

    fn test_context() -> PromptContext {
        PromptContext {
            working_dir: "/workspace/demo".to_string(),
            tool_names: Vec::new(),
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
    async fn layered_builder_merges_non_empty_plan_and_marks_layers() {
        let builder = LayeredPromptBuilder::new()
            .with_stable_layer(vec![Arc::new(StaticContributor {
                id: "stable",
                block_id: "stable-block",
                title: "Stable",
                content: "stable content",
            })])
            .with_semi_stable_layer(vec![Arc::new(StaticContributor {
                id: "semi",
                block_id: "semi-block",
                title: "Semi",
                content: "semi content",
            })])
            .with_inherited_layer(vec![Arc::new(StaticContributor {
                id: "inherited",
                block_id: "inherited-block",
                title: "Inherited",
                content: "inherited content",
            })])
            .with_dynamic_layer(vec![Arc::new(StaticContributor {
                id: "dynamic",
                block_id: "dynamic-block",
                title: "Dynamic",
                content: "dynamic content",
            })]);

        let output = builder
            .build(&test_context())
            .await
            .expect("layered build should succeed");

        assert_eq!(output.plan.system_blocks.len(), 4);
        assert_eq!(
            output
                .plan
                .ordered_system_blocks()
                .into_iter()
                .map(|block| block.layer)
                .collect::<Vec<_>>(),
            vec![
                PromptLayer::Stable,
                PromptLayer::SemiStable,
                PromptLayer::Inherited,
                PromptLayer::Dynamic
            ]
        );
    }

    #[test]
    fn stable_zero_ttl_never_expires() {
        let entry = LayerCacheEntry {
            fingerprint: "fp".to_string(),
            cached_at: Instant::now(),
            output: PromptBuildOutput {
                plan: PromptPlan::default(),
                diagnostics: PromptDiagnostics::default(),
                cache_hints: PromptCacheHints::default(),
            },
        };
        let options = LayeredBuilderOptions {
            stable_cache_ttl: Duration::ZERO,
            ..Default::default()
        };

        assert!(!is_cache_expired(&entry, &options, LayerType::Stable));
    }
}
