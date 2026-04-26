//! Prompt cache 断点诊断。
//!
//! 采用两阶段检测：
//! - 请求发送前记录一次 prompt/tool/cache 策略快照
//! - 响应返回后根据真实 `cache_read_input_tokens` 跌幅判断是否发生 cache break

use astrcode_runtime_contract::llm::{
    LlmUsage, PromptCacheBreakReason, PromptCacheDiagnostics, PromptCacheGlobalStrategy,
};
use serde::Serialize;

const MIN_CACHE_DROP_TOKENS: usize = 2_000;

#[derive(Debug, Clone)]
pub(crate) struct CacheCheckContext {
    pub(crate) system_blocks_hash: String,
    pub(crate) tool_schema_hash: String,
    pub(crate) model: String,
    pub(crate) global_cache_strategy: PromptCacheGlobalStrategy,
    pub(crate) compacted: bool,
    pub(crate) tool_result_rebudgeted: bool,
}

#[derive(Debug, Clone)]
pub(crate) struct PendingCacheCheck {
    snapshot: CacheSnapshot,
    reasons: Vec<PromptCacheBreakReason>,
    previous_cache_read_input_tokens: Option<usize>,
    expected_drop: bool,
}

#[derive(Debug, Clone, Default)]
pub struct CacheTracker {
    previous: Option<CompletedCacheSnapshot>,
}

#[derive(Debug, Clone)]
struct CompletedCacheSnapshot {
    snapshot: CacheSnapshot,
    cache_read_input_tokens: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CacheSnapshot {
    system_blocks_hash: String,
    tool_schema_hash: String,
    model: String,
    global_cache_strategy: PromptCacheGlobalStrategy,
    compacted: bool,
    tool_result_rebudgeted: bool,
}

impl CacheTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub(crate) fn prepare(&self, context: &CacheCheckContext) -> PendingCacheCheck {
        let snapshot = CacheSnapshot::from_context(context);
        let mut reasons = Vec::new();
        let mut previous_cache_read_input_tokens = None;

        if let Some(previous) = &self.previous {
            previous_cache_read_input_tokens = previous.cache_read_input_tokens;
            if previous.snapshot.system_blocks_hash != snapshot.system_blocks_hash {
                reasons.push(PromptCacheBreakReason::SystemPromptChanged);
            }
            if previous.snapshot.tool_schema_hash != snapshot.tool_schema_hash {
                reasons.push(PromptCacheBreakReason::ToolSchemasChanged);
            }
            if previous.snapshot.model != snapshot.model {
                reasons.push(PromptCacheBreakReason::ModelChanged);
            }
            if previous.snapshot.global_cache_strategy != snapshot.global_cache_strategy {
                reasons.push(PromptCacheBreakReason::GlobalCacheStrategyChanged);
            }
        }

        let expected_drop = snapshot.compacted || snapshot.tool_result_rebudgeted;
        if snapshot.compacted {
            reasons.push(PromptCacheBreakReason::CompactedPrompt);
        }
        if snapshot.tool_result_rebudgeted {
            reasons.push(PromptCacheBreakReason::ToolResultRebudgeted);
        }

        PendingCacheCheck {
            snapshot,
            reasons,
            previous_cache_read_input_tokens,
            expected_drop,
        }
    }

    pub(crate) fn finalize(
        &mut self,
        pending: PendingCacheCheck,
        usage: Option<LlmUsage>,
    ) -> Option<PromptCacheDiagnostics> {
        let current_cache_read_input_tokens = usage.map(|usage| usage.cache_read_input_tokens);
        let cache_break_detected = matches!(
            (
                pending.previous_cache_read_input_tokens,
                current_cache_read_input_tokens,
            ),
            (Some(previous), Some(current))
                if previous > current
                    && previous.saturating_sub(current) >= MIN_CACHE_DROP_TOKENS
                    && !pending.expected_drop
        );

        self.previous = Some(CompletedCacheSnapshot {
            snapshot: pending.snapshot,
            cache_read_input_tokens: current_cache_read_input_tokens,
        });

        if pending.reasons.is_empty()
            && pending.previous_cache_read_input_tokens.is_none()
            && current_cache_read_input_tokens.is_none()
        {
            return None;
        }

        Some(PromptCacheDiagnostics {
            reasons: pending.reasons,
            previous_cache_read_input_tokens: pending.previous_cache_read_input_tokens,
            current_cache_read_input_tokens,
            expected_drop: pending.expected_drop,
            cache_break_detected,
        })
    }
}

impl CacheSnapshot {
    fn from_context(context: &CacheCheckContext) -> Self {
        Self {
            system_blocks_hash: context.system_blocks_hash.clone(),
            tool_schema_hash: context.tool_schema_hash.clone(),
            model: context.model.clone(),
            global_cache_strategy: context.global_cache_strategy,
            compacted: context.compacted,
            tool_result_rebudgeted: context.tool_result_rebudgeted,
        }
    }
}

pub(crate) fn stable_hash<T>(value: &T) -> String
where
    T: Serialize,
{
    use std::{
        collections::hash_map::DefaultHasher,
        hash::{Hash, Hasher},
    };

    let rendered = serde_json::to_string(value)
        .unwrap_or_else(|_| format!("{:?}", std::any::type_name::<T>()));
    let mut hasher = DefaultHasher::new();
    rendered.hash(&mut hasher);
    format!("{:x}", hasher.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context() -> CacheCheckContext {
        CacheCheckContext {
            system_blocks_hash: "system-a".to_string(),
            tool_schema_hash: "tools-a".to_string(),
            model: "gpt-4.1".to_string(),
            global_cache_strategy: PromptCacheGlobalStrategy::SystemPrompt,
            compacted: false,
            tool_result_rebudgeted: false,
        }
    }

    #[test]
    fn finalize_reports_real_cache_breaks() {
        let mut tracker = CacheTracker::new();
        let first = tracker.prepare(&context());
        let _ = tracker.finalize(
            first,
            Some(LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 12_000,
            }),
        );

        let mut changed_context = context();
        changed_context.model = "gpt-4.1-mini".to_string();
        let second = tracker.prepare(&changed_context);
        let diagnostics = tracker
            .finalize(
                second,
                Some(LlmUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 3_000,
                }),
            )
            .expect("diagnostics should exist");

        assert!(diagnostics.cache_break_detected);
        assert!(
            diagnostics
                .reasons
                .contains(&PromptCacheBreakReason::ModelChanged)
        );
    }

    #[test]
    fn finalize_treats_compaction_drop_as_expected() {
        let mut tracker = CacheTracker::new();
        let first = tracker.prepare(&context());
        let _ = tracker.finalize(
            first,
            Some(LlmUsage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 10_000,
            }),
        );

        let mut compacted_context = context();
        compacted_context.compacted = true;
        let second = tracker.prepare(&compacted_context);
        let diagnostics = tracker
            .finalize(
                second,
                Some(LlmUsage {
                    input_tokens: 100,
                    output_tokens: 50,
                    cache_creation_input_tokens: 0,
                    cache_read_input_tokens: 1_000,
                }),
            )
            .expect("diagnostics should exist");

        assert!(!diagnostics.cache_break_detected);
        assert!(diagnostics.expected_drop);
        assert!(
            diagnostics
                .reasons
                .contains(&PromptCacheBreakReason::CompactedPrompt)
        );
    }
}
