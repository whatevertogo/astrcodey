//! Memory pipeline — 提取管线 + TurnEnd 增量提取。
//!
//! SessionStart pipeline: 从符合条件的历史会话中提取记忆，写入 contexts/ 目录。
//! TurnEnd 增量提取: 召回相关历史上下文辅助当前 turn 提取记忆。
//! MEMORY.md 不由 pipeline 修改，仅由 LLM 通过工具操作。

use std::sync::Arc;

use astrcode_core::{
    capability::{
        ConversationView, EventQueryCap, LlmInvokerCap, PromptRole, SessionSummaryView,
    },
    extension::ExtensionError,
};
use chrono::{DateTime, Duration, Local, Utc};

use crate::{
    pipeline_prompts,
    store::{MemoryStore, Phase1Output, ProcessedSession},
};

// ─── Config ─────────────────────────────────────────────────────────────

struct PipelineConfig {
    max_candidates: usize,
    min_idle_minutes: i64,
    max_context_age_days: u64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            max_candidates: 5,
            min_idle_minutes: 30,
            max_context_age_days: 90,
        }
    }
}

#[derive(Clone)]
struct Candidate {
    summary: SessionSummaryView,
    updated_at: DateTime<Utc>,
}

// ─── Pipeline Entry ─────────────────────────────────────────────────────

pub async fn run(
    store: &MemoryStore,
    event_query: Arc<EventQueryCap>,
    llm_invoker: Arc<LlmInvokerCap>,
    current_session_id: &str,
) -> Result<(), ExtensionError> {
    let config = PipelineConfig::default();

    // 清理过期的 contexts/ 文件
    if let Ok(n) = store.cleanup_old_contexts(config.max_context_age_days) {
        if n > 0 {
            tracing::debug!(deleted = n, "cleaned up old context files");
        }
    }

    let candidates = find_candidates(event_query.clone(), store, current_session_id, &config).await?;
    if candidates.is_empty() {
        return Ok(());
    }

    let extractions = extract(event_query, llm_invoker, &candidates).await?;

    if extractions.is_empty() {
        return Ok(());
    }

    // 写入 contexts/ 目录，不动 MEMORY.md
    write_contexts(store, &extractions)
}

// ─── Candidate Selection ────────────────────────────────────────────────

async fn find_candidates(
    event_query: Arc<EventQueryCap>,
    store: &MemoryStore,
    current_session_id: &str,
    config: &PipelineConfig,
) -> Result<Vec<Candidate>, ExtensionError> {
    let summaries = event_query
        .list_session_summaries()
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;
    let processed = store
        .list_processed()
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;
    let cutoff = Utc::now() - Duration::minutes(config.min_idle_minutes);

    let mut candidates: Vec<Candidate> = summaries
        .into_iter()
        .filter_map(|s| {
            if s.session_id == current_session_id {
                return None;
            }
            if s.parent_session_id.is_some() {
                return None;
            }
            if s.source_extension.is_some() {
                return None;
            }
            let updated_at_str = s.updated_at.as_ref()?;
            if processed.get(&s.session_id) == Some(updated_at_str) {
                return None;
            }
            let updated = DateTime::parse_from_rfc3339(updated_at_str).ok()?;
            let updated_utc = updated.with_timezone(&Utc);
            let idle_enough = updated_utc < cutoff;
            let has_enough_content = s.first_user_message.as_ref().is_some_and(|m| m.len() >= 50);
            if !idle_enough && !has_enough_content {
                return None;
            }
            Some(Candidate {
                summary: s,
                updated_at: updated_utc,
            })
        })
        .collect();

    candidates.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    candidates.truncate(config.max_candidates);

    Ok(candidates)
}

// ─── Phase 1: Extraction ────────────────────────────────────────────────

async fn extract(
    event_query: Arc<EventQueryCap>,
    llm_invoker: Arc<LlmInvokerCap>,
    candidates: &[Candidate],
) -> Result<Vec<(Candidate, Phase1Output)>, ExtensionError> {
    let mut results = Vec::new();
    for candidate in candidates {
        let session_id = &candidate.summary.session_id;
        let conversation_view = event_query
            .read_conversation(session_id)
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        let conversation = extract_conversation(&conversation_view);
        if conversation.is_empty() {
            results.push((
                candidate.clone(),
                Phase1Output {
                    memories: Vec::new(),
                },
            ));
            continue;
        }

        let current_date = Local::now().format("%Y-%m-%d").to_string();
        let prompt = pipeline_prompts::phase1_user_prompt(&conversation, &current_date);

        let text = llm_invoker
            .invoke_complete(pipeline_prompts::PHASE1_SYSTEM.to_string(), prompt)
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        match parse_phase1_output(&text) {
            Ok(output) if output.memories.is_empty() => {
                results.push((candidate.clone(), output));
            },
            Ok(output) => {
                results.push((candidate.clone(), output));
            },
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    session_id = %session_id,
                    "memory pipeline: failed to parse extraction"
                );
            },
        }
    }
    Ok(results)
}

fn extract_conversation(view: &ConversationView) -> String {
    // Collect all text turns first, then apply middle truncation.
    const MAX_BYTES: usize = 4000; // ~1000 tokens at 4 bytes/token
    const MAX_TURNS: usize = 30;

    let turns: Vec<String> = view
        .turns
        .iter()
        .filter_map(|turn| {
            let role = match turn.role {
                PromptRole::User => "User",
                PromptRole::Assistant => "Assistant",
                PromptRole::System => return None,
            };
            if turn.text.is_empty() {
                None
            } else {
                Some(format!("{role}: {}", turn.text))
            }
        })
        .take(MAX_TURNS)
        .collect();

    if turns.is_empty() {
        return String::new();
    }

    let total_bytes: usize = turns.iter().map(|t| t.len()).sum();
    if total_bytes <= MAX_BYTES {
        return turns.join("\n\n");
    }

    // Middle truncation: split budget 50/50 between head and tail.
    let budget = MAX_BYTES / 2;
    let mut head = Vec::new();
    let mut head_bytes = 0;
    let mut tail = Vec::new();
    let mut tail_bytes = 0;

    for turn in &turns {
        if head_bytes + turn.len() <= budget {
            head.push(turn.as_str());
            head_bytes += turn.len();
        } else {
            break;
        }
    }

    for turn in turns.iter().rev() {
        if tail_bytes + turn.len() <= budget {
            tail.push(turn.as_str());
            tail_bytes += turn.len();
        } else {
            break;
        }
    }
    tail.reverse();

    let truncated_count = turns.len() - head.len() - tail.len();
    let marker = format!("… {truncated_count} turns truncated …");
    let mut result = head;
    result.push(marker.as_str());
    result.extend(tail);
    result.join("\n\n")
}

fn parse_phase1_output(text: &str) -> Result<Phase1Output, ExtensionError> {
    let text = text.trim();
    let json_str = text
        .strip_prefix("```json")
        .and_then(|s| s.strip_suffix("```"))
        .map(|s| s.trim())
        .unwrap_or(text);
    serde_json::from_str(json_str)
        .map_err(|e| ExtensionError::Internal(format!("parse extraction: {e}")))
}

// ─── Write Contexts ────────────────────────────────────────────────────

/// 将提取结果写入 contexts/ 目录，不动 MEMORY.md。
fn write_contexts(
    store: &MemoryStore,
    extractions: &[(Candidate, Phase1Output)],
) -> Result<(), ExtensionError> {
    let processed: Vec<ProcessedSession> = extractions
        .iter()
        .map(|(c, _)| ProcessedSession {
            session_id: c.summary.session_id.clone(),
            updated_at: c.summary.updated_at.clone().unwrap_or_default(),
        })
        .collect();

    let context_files: Vec<(String, String)> = extractions
        .iter()
        .filter(|(_, output)| !output.memories.is_empty())
        .map(|(candidate, output)| {
            let memories: String = output
                .memories
                .iter()
                .map(|m| format!("- [{}] {}", m.category, m.content))
                .collect::<Vec<_>>()
                .join("\n");
            let filename = format!("{}.md", candidate.summary.session_id);
            let content = format!(
                "# Session {}\n\n## Extracted Memories\n{}",
                candidate.summary.session_id, memories
            );
            (filename, content)
        })
        .collect();

    store
        .commit_pipeline_result(&processed, &context_files)
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    Ok(())
}

// ─── TurnEnd Incremental Extraction ────────────────────────────────────

/// TurnEnd 增量提取：读取已有记忆 → 召回历史上下文 → 小模型提取 → 写入 contexts/。
pub async fn extract_turn(
    store: Arc<MemoryStore>,
    llm_invoker: &LlmInvokerCap,
    session_id: &str,
    user_message: &str,
    assistant_message: &str,
    recalled_contexts: &[String],
) -> Result<(), ExtensionError> {
    // 读取已有 MEMORY.md 用于 prompt 内去重
    let existing_memory = tokio::task::spawn_blocking({
        let store = store.clone();
        move || store.read_memory()
    })
    .await
    .map_err(|e| ExtensionError::Internal(e.to_string()))?
    .unwrap_or_default();

    let prompt = pipeline_prompts::turn_extract_prompt(
        user_message,
        assistant_message,
        &existing_memory,
        recalled_contexts,
        &Local::now().format("%Y-%m-%d").to_string(),
    );

    let text = llm_invoker
        .invoke_complete(pipeline_prompts::PHASE1_SYSTEM.to_string(), prompt)
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    let output = parse_phase1_output(&text)?;

    if output.memories.is_empty() {
        return Ok(());
    }

    // Hash 去重：精确匹配兜底
    let existing_hashes = tokio::task::spawn_blocking({
        let store = store.clone();
        move || store.existing_entry_hashes()
    })
    .await
    .map_err(|e| ExtensionError::Internal(e.to_string()))?
    .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    let new_memories: Vec<_> = output
        .memories
        .into_iter()
        .filter(|m| {
            let normalized = m.content.to_lowercase();
            let normalized = normalized.split_whitespace().collect::<Vec<_>>().join(" ");
            let hash = astrcode_support::hash::fnv1a_hash_bytes(normalized.as_bytes());
            !existing_hashes.contains(&hash)
        })
        .collect();

    if new_memories.is_empty() {
        return Ok(());
    }

    // 写入 contexts/ 文件
    let filename = format!(
        "{session_id}-turn-{}.md",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let memories_text: String = new_memories
        .iter()
        .map(|m| format!("- [{}] {}", m.category, m.content))
        .collect::<Vec<_>>()
        .join("\n");
    let content =
        format!("# Session {session_id} (incremental)\n\n## Extracted Memories\n{memories_text}");

    tokio::task::spawn_blocking(move || store.commit_pipeline_result(&[], &[(filename, content)]))
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    Ok(())
}
