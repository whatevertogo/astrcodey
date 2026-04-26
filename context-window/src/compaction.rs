use std::{collections::HashSet, sync::OnceLock};

use astrcode_core::{
    AstrError, CancelToken, CompactAppliedMeta, CompactMode, CompactSummaryEnvelope,
    CompactTrigger, LlmMessage, Result, StorageEvent, StorageEventPayload, UserMessageOrigin,
    format_compact_summary, parse_compact_summary_message,
};
use astrcode_runtime_contract::{
    RuntimeTurnEvent,
    llm::{LlmProvider, LlmRequest, ModelLimits},
};
use chrono::{DateTime, Utc};
use regex::Regex;

use super::{
    file_access::FileAccessTracker,
    settings::ContextWindowSettings,
    token_usage::{effective_context_window, estimate_request_tokens},
};

const BASE_COMPACT_PROMPT_TEMPLATE: &str = include_str!("templates/compact/base.md");
const INCREMENTAL_COMPACT_PROMPT_TEMPLATE: &str = include_str!("templates/compact/incremental.md");

#[path = "compaction/protocol.rs"]
mod protocol;
use protocol::*;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompactConfig {
    pub keep_recent_turns: usize,
    pub keep_recent_user_messages: usize,
    pub trigger: CompactTrigger,
    pub summary_reserve_tokens: usize,
    pub max_output_tokens: usize,
    pub max_retry_attempts: usize,
    pub history_path: Option<String>,
    pub custom_instructions: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CompactResult {
    pub messages: Vec<LlmMessage>,
    pub summary: String,
    pub recent_user_context_digest: Option<String>,
    pub recent_user_context_messages: Vec<String>,
    pub preserved_recent_turns: usize,
    pub pre_tokens: usize,
    pub post_tokens_estimate: usize,
    pub messages_removed: usize,
    pub tokens_freed: usize,
    pub timestamp: DateTime<Utc>,
    pub meta: CompactAppliedMeta,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CompactionBoundary {
    RealUserTurn,
    AssistantStep,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct CompactionUnit {
    start: usize,
    boundary: CompactionBoundary,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CompactPromptMode {
    Fresh,
    Incremental { previous_summary: String },
}

impl CompactPromptMode {
    fn compact_mode(&self, retry_count: usize) -> CompactMode {
        if retry_count > 0 {
            CompactMode::RetrySalvage
        } else if matches!(self, Self::Incremental { .. }) {
            CompactMode::Incremental
        } else {
            CompactMode::Full
        }
    }
}

#[derive(Debug, Clone)]
struct PreparedCompactInput {
    messages: Vec<LlmMessage>,
    prompt_mode: CompactPromptMode,
    input_units: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CompactContractViolation {
    detail: String,
}

impl CompactContractViolation {
    fn from_parsed_output(parsed: &ParsedCompactOutput) -> Option<Self> {
        if parsed.used_fallback {
            return Some(Self {
                detail: "response did not contain a strict <summary> XML block and required \
                         fallback parsing"
                    .to_string(),
            });
        }
        if !parsed.has_analysis {
            return Some(Self {
                detail: "response omitted the required <analysis> block".to_string(),
            });
        }
        if !parsed.has_recent_user_context_digest_block {
            return Some(Self {
                detail: "response omitted the required <recent_user_context_digest> block"
                    .to_string(),
            });
        }
        None
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CompactRetryState {
    salvage_attempts: usize,
    contract_retry_count: usize,
    contract_repair_feedback: Option<String>,
}

impl CompactRetryState {
    fn schedule_contract_retry(&mut self, detail: String) {
        self.contract_retry_count = self.contract_retry_count.saturating_add(1);
        self.contract_repair_feedback = Some(detail);
    }

    fn note_salvage_attempt(&mut self) {
        self.salvage_attempts = self.salvage_attempts.saturating_add(1);
    }
}

#[derive(Debug, Clone)]
struct CompactExecutionResult {
    parsed_output: ParsedCompactOutput,
    prepared_input: PreparedCompactInput,
    retry_state: CompactRetryState,
}

pub async fn auto_compact(
    provider: &dyn LlmProvider,
    messages: &[LlmMessage],
    compact_prompt_context: Option<&str>,
    config: CompactConfig,
    cancel: CancelToken,
) -> Result<Option<CompactResult>> {
    let recent_user_context_messages =
        collect_recent_user_context_messages(messages, config.keep_recent_user_messages);
    let preserved_recent_turns = config
        .keep_recent_turns
        .max(config.keep_recent_user_messages)
        .max(1);
    let Some(mut split) = split_for_compaction(messages, preserved_recent_turns) else {
        return Ok(None);
    };

    let pre_tokens = estimate_request_tokens(messages, compact_prompt_context);
    let effective_max_output_tokens = config
        .max_output_tokens
        .min(provider.model_limits().max_output_tokens)
        .max(1);
    let Some(execution) = execute_compact_request_with_retries(
        provider,
        &mut split,
        compact_prompt_context,
        &config,
        &recent_user_context_messages,
        effective_max_output_tokens,
        cancel,
    )
    .await?
    else {
        return Ok(None);
    };

    let summary = {
        let summary = sanitize_compact_summary(&execution.parsed_output.summary);
        if let Some(history_path) = config.history_path.as_deref() {
            CompactSummaryEnvelope::new(summary)
                .with_history_path(history_path)
                .render_body()
        } else {
            summary
        }
    };
    let recent_user_context_digest = execution
        .parsed_output
        .recent_user_context_digest
        .as_deref()
        .map(sanitize_recent_user_context_digest)
        .filter(|value| !value.is_empty());
    let compacted_messages = compacted_messages(
        &summary,
        recent_user_context_digest.as_deref(),
        &recent_user_context_messages,
        split.keep_start,
        split.suffix,
    );

    Ok(Some(build_compact_result(
        CompactResultInput {
            compacted_messages,
            summary,
            recent_user_context_digest,
            recent_user_context_messages,
            preserved_recent_turns,
            pre_tokens,
            messages_removed: split.keep_start,
        },
        compact_prompt_context,
        &config,
        execution,
    )))
}

pub fn build_post_compact_events(
    turn_id: Option<&str>,
    agent: &astrcode_core::AgentEventContext,
    trigger: CompactTrigger,
    compaction: &CompactResult,
) -> Vec<RuntimeTurnEvent> {
    let _ = (
        &compaction.recent_user_context_digest,
        &compaction.recent_user_context_messages,
    );
    vec![RuntimeTurnEvent::StorageEvent {
        event: Box::new(StorageEvent {
            turn_id: turn_id.map(str::to_string),
            agent: agent.clone(),
            payload: StorageEventPayload::CompactApplied {
                trigger,
                summary: compaction.summary.clone(),
                meta: compaction.meta.clone(),
                preserved_recent_turns: saturating_u32(compaction.preserved_recent_turns),
                pre_tokens: saturating_u32(compaction.pre_tokens),
                post_tokens_estimate: saturating_u32(compaction.post_tokens_estimate),
                messages_removed: saturating_u32(compaction.messages_removed),
                tokens_freed: saturating_u32(compaction.tokens_freed),
                timestamp: compaction.timestamp,
            },
        }),
    }]
}

pub fn build_post_compact_recovery_messages(
    tracker: &FileAccessTracker,
    settings: &ContextWindowSettings,
) -> Vec<LlmMessage> {
    tracker.build_recovery_messages(settings.file_recovery_config())
}

pub fn compact_config_from_settings(
    settings: &ContextWindowSettings,
    trigger: CompactTrigger,
    history_path: Option<String>,
    custom_instructions: Option<String>,
) -> CompactConfig {
    CompactConfig {
        keep_recent_turns: settings.compact_keep_recent_turns,
        keep_recent_user_messages: settings.compact_keep_recent_user_messages,
        trigger,
        summary_reserve_tokens: settings.summary_reserve_tokens,
        max_output_tokens: settings.compact_max_output_tokens,
        max_retry_attempts: settings.compact_max_retry_attempts,
        history_path,
        custom_instructions,
    }
}

pub fn is_prompt_too_long_message(message: &str) -> bool {
    contains_ascii_case_insensitive(message, "prompt too long")
        || contains_ascii_case_insensitive(message, "context length")
        || contains_ascii_case_insensitive(message, "maximum context")
        || contains_ascii_case_insensitive(message, "too many tokens")
}

struct CompactionSplit {
    prefix: Vec<LlmMessage>,
    suffix: Vec<LlmMessage>,
    keep_start: usize,
}

fn split_for_compaction(
    messages: &[LlmMessage],
    keep_recent_turns: usize,
) -> Option<CompactionSplit> {
    if messages.is_empty() {
        return None;
    }

    let real_user_indices = real_user_turn_indices(messages);
    let primary_keep_start = real_user_indices
        .len()
        .checked_sub(keep_recent_turns.max(1))
        .map(|index| real_user_indices[index]);
    let keep_start = primary_keep_start
        .filter(|index| *index > 0)
        .or_else(|| fallback_keep_start(messages))?;
    Some(CompactionSplit {
        prefix: messages[..keep_start].to_vec(),
        suffix: messages[keep_start..].to_vec(),
        keep_start,
    })
}

fn real_user_turn_indices(messages: &[LlmMessage]) -> Vec<usize> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message {
            LlmMessage::User {
                origin: UserMessageOrigin::User,
                ..
            } => Some(index),
            _ => None,
        })
        .collect()
}

fn fallback_keep_start(messages: &[LlmMessage]) -> Option<usize> {
    compaction_units(messages)
        .into_iter()
        .rev()
        .find(|unit| unit.boundary == CompactionBoundary::AssistantStep && unit.start > 0)
        .map(|unit| unit.start)
}

fn compaction_units(messages: &[LlmMessage]) -> Vec<CompactionUnit> {
    messages
        .iter()
        .enumerate()
        .filter_map(|(index, message)| match message {
            LlmMessage::User {
                origin: UserMessageOrigin::User,
                ..
            } => Some(CompactionUnit {
                start: index,
                boundary: CompactionBoundary::RealUserTurn,
            }),
            LlmMessage::Assistant { .. } => Some(CompactionUnit {
                start: index,
                boundary: CompactionBoundary::AssistantStep,
            }),
            _ => None,
        })
        .collect()
}

fn drop_oldest_compaction_unit(prefix: &mut Vec<LlmMessage>) -> bool {
    let mut boundary_starts =
        prefix
            .iter()
            .enumerate()
            .filter_map(|(index, message)| match message {
                LlmMessage::User {
                    origin: UserMessageOrigin::User,
                    ..
                }
                | LlmMessage::Assistant { .. } => Some(index),
                _ => None,
            });
    let _current_start = boundary_starts.next();
    let Some(next_start) = boundary_starts.next() else {
        prefix.clear();
        return false;
    };
    if next_start == 0 || next_start >= prefix.len() {
        prefix.clear();
        return false;
    }

    prefix.drain(..next_start);
    !prefix.is_empty()
}

fn trim_prefix_until_compact_request_fits(
    prefix: &mut Vec<LlmMessage>,
    compact_prompt_context: Option<&str>,
    limits: ModelLimits,
    config: &CompactConfig,
    recent_user_context_messages: &[RecentUserContextMessage],
) -> bool {
    loop {
        let prepared_input = prepare_compact_input(prefix);
        if prepared_input.messages.is_empty() {
            return false;
        }

        let system_prompt = render_compact_system_prompt(
            compact_prompt_context,
            prepared_input.prompt_mode,
            config
                .max_output_tokens
                .min(limits.max_output_tokens)
                .max(1),
            recent_user_context_messages,
            config.custom_instructions.as_deref(),
            None,
        );
        if compact_request_fits_window(
            &prepared_input.messages,
            &system_prompt,
            limits,
            config.summary_reserve_tokens,
        ) {
            return true;
        }

        if !drop_oldest_compaction_unit(prefix) {
            return false;
        }
    }
}

async fn execute_compact_request_with_retries(
    provider: &dyn LlmProvider,
    split: &mut CompactionSplit,
    compact_prompt_context: Option<&str>,
    config: &CompactConfig,
    recent_user_context_messages: &[RecentUserContextMessage],
    effective_max_output_tokens: usize,
    cancel: CancelToken,
) -> Result<Option<CompactExecutionResult>> {
    let mut retry_state = CompactRetryState::default();
    loop {
        if !trim_prefix_until_compact_request_fits(
            &mut split.prefix,
            compact_prompt_context,
            provider.model_limits(),
            config,
            recent_user_context_messages,
        ) {
            return Err(AstrError::Internal(
                "compact request could not fit within summarization window".to_string(),
            ));
        }

        let prepared_input = prepare_compact_input(&split.prefix);
        if prepared_input.messages.is_empty() {
            return Ok(None);
        }

        let request = LlmRequest::new(prepared_input.messages.clone(), Vec::new(), cancel.clone())
            .with_system(render_compact_system_prompt(
                compact_prompt_context,
                prepared_input.prompt_mode.clone(),
                effective_max_output_tokens,
                recent_user_context_messages,
                config.custom_instructions.as_deref(),
                retry_state.contract_repair_feedback.as_deref(),
            ))
            .with_max_output_tokens_override(effective_max_output_tokens);

        match provider.generate(request, None).await {
            Ok(output) => match parse_compact_output(&output.content) {
                Ok(parsed_output) => {
                    if let Some(violation) =
                        CompactContractViolation::from_parsed_output(&parsed_output)
                    {
                        if retry_state.contract_retry_count < config.max_retry_attempts {
                            retry_state.schedule_contract_retry(violation.detail);
                            continue;
                        }
                    }
                    return Ok(Some(CompactExecutionResult {
                        parsed_output,
                        prepared_input,
                        retry_state,
                    }));
                },
                Err(error) if retry_state.contract_retry_count < config.max_retry_attempts => {
                    retry_state.schedule_contract_retry(error.to_string());
                    continue;
                },
                Err(error) => return Err(error),
            },
            Err(error)
                if is_prompt_too_long_message(&error.to_string())
                    && retry_state.salvage_attempts < config.max_retry_attempts =>
            {
                retry_state.note_salvage_attempt();
                if !drop_oldest_compaction_unit(&mut split.prefix) {
                    return Err(AstrError::Internal(error.to_string()));
                }
                split.keep_start = split.prefix.len();
            },
            Err(error) => return Err(AstrError::Internal(error.to_string())),
        }
    }
}

struct CompactResultInput {
    compacted_messages: Vec<LlmMessage>,
    summary: String,
    recent_user_context_digest: Option<String>,
    recent_user_context_messages: Vec<RecentUserContextMessage>,
    preserved_recent_turns: usize,
    pre_tokens: usize,
    messages_removed: usize,
}

fn build_compact_result(
    input: CompactResultInput,
    compact_prompt_context: Option<&str>,
    _config: &CompactConfig,
    execution: CompactExecutionResult,
) -> CompactResult {
    let CompactResultInput {
        compacted_messages,
        summary,
        recent_user_context_digest,
        recent_user_context_messages,
        preserved_recent_turns,
        pre_tokens,
        messages_removed,
    } = input;
    let CompactExecutionResult {
        parsed_output,
        prepared_input,
        retry_state,
    } = execution;
    let post_tokens_estimate = estimate_request_tokens(&compacted_messages, compact_prompt_context);
    let output_summary_chars = summary.chars().count().min(u32::MAX as usize) as u32;

    CompactResult {
        messages: compacted_messages,
        summary,
        recent_user_context_digest,
        recent_user_context_messages: recent_user_context_messages
            .into_iter()
            .map(|message| message.content)
            .collect(),
        preserved_recent_turns,
        pre_tokens,
        post_tokens_estimate,
        messages_removed,
        tokens_freed: pre_tokens.saturating_sub(post_tokens_estimate),
        timestamp: Utc::now(),
        meta: CompactAppliedMeta {
            mode: prepared_input
                .prompt_mode
                .compact_mode(retry_state.salvage_attempts),
            instructions_present: false,
            fallback_used: parsed_output.used_fallback || retry_state.salvage_attempts > 0,
            retry_count: retry_state.salvage_attempts.min(u32::MAX as usize) as u32,
            input_units: prepared_input.input_units.min(u32::MAX as usize) as u32,
            output_summary_chars,
        },
    }
}

fn compact_request_fits_window(
    request_messages: &[LlmMessage],
    system_prompt: &str,
    limits: ModelLimits,
    summary_reserve_tokens: usize,
) -> bool {
    estimate_request_tokens(request_messages, Some(system_prompt))
        <= effective_context_window(limits, summary_reserve_tokens)
}

fn compacted_messages(
    summary: &str,
    recent_user_context_digest: Option<&str>,
    recent_user_context_messages: &[RecentUserContextMessage],
    keep_start: usize,
    suffix: Vec<LlmMessage>,
) -> Vec<LlmMessage> {
    let recent_user_context_indices = recent_user_context_messages
        .iter()
        .map(|message| message.index)
        .collect::<HashSet<_>>();
    let mut messages = vec![LlmMessage::User {
        content: format_compact_summary(summary),
        origin: UserMessageOrigin::CompactSummary,
    }];
    if let Some(digest) = recent_user_context_digest.filter(|value| !value.trim().is_empty()) {
        messages.push(LlmMessage::User {
            content: digest.trim().to_string(),
            origin: UserMessageOrigin::RecentUserContextDigest,
        });
    }
    for message in recent_user_context_messages {
        messages.push(LlmMessage::User {
            content: message.content.clone(),
            origin: UserMessageOrigin::RecentUserContext,
        });
    }
    messages.extend(
        suffix
            .into_iter()
            .enumerate()
            .filter(|(offset, message)| {
                let is_reinjected_real_user_message = matches!(
                    message,
                    LlmMessage::User {
                        origin: UserMessageOrigin::User,
                        ..
                    }
                ) && recent_user_context_indices
                    .contains(&(keep_start + offset));
                !is_reinjected_real_user_message
            })
            .map(|(_, message)| message),
    );
    messages
}

fn saturating_u32(value: usize) -> u32 {
    value.min(u32::MAX as usize) as u32
}
