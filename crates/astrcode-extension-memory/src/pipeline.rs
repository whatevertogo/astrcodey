//! Memory pipeline — SessionStart / post-`memory_save` batch extraction from changed rollouts.

use std::sync::Arc;

use astrcode_extension_sdk::{
    extension::ExtensionError,
    llm::{LlmContent, LlmMessage, LlmProvider, LlmRole},
    storage::{EventReader, SessionReadModel, SessionSummary},
};
use chrono::{DateTime, Local, Utc};
use serde::Deserialize;

use crate::{
    config::MemoryConfig,
    prompts,
    scope::ScopedMemoryStores,
    store::{MemoryEntry, MemoryStore, Phase1Output, ProcessedSession},
};

#[derive(Clone)]
struct Candidate {
    summary: SessionSummary,
    updated_at: DateTime<Utc>,
}

#[derive(Debug, Deserialize)]
struct BatchPhase1Output {
    #[serde(default)]
    sessions: Vec<SessionExtraction>,
}

#[derive(Debug, Deserialize)]
struct SessionExtraction {
    session_id: String,
    #[serde(default)]
    memories: Vec<MemoryEntry>,
}

pub async fn run(
    scoped: &ScopedMemoryStores,
    session_read: Arc<dyn EventReader>,
    small_llm: &dyn LlmProvider,
    current_session_id: &str,
    config: &MemoryConfig,
) -> Result<(), ExtensionError> {
    let store = scoped.project.as_ref();

    if let Ok(n) = store.cleanup_old_contexts(config.max_context_age_days) {
        if n > 0 {
            tracing::debug!(deleted = n, "cleaned up old context files");
        }
    }

    let candidates = find_changed_candidates(
        Arc::clone(&session_read),
        store,
        current_session_id,
        config.max_changed_sessions,
    )
    .await?;
    if candidates.is_empty() {
        return Ok(());
    }

    let existing_memory = tokio::task::spawn_blocking({
        let user = scoped.user.clone();
        let project = scoped.project.clone();
        move || format_existing_memories(&user, &project, 6000)
    })
    .await
    .map_err(|e| ExtensionError::Internal(e.to_string()))?
    .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    let extractions = extract_batch(
        Arc::clone(&session_read),
        small_llm,
        &candidates,
        &existing_memory,
        config.min_conversation_chars,
    )
    .await?;

    if !extractions.is_empty() {
        write_contexts(store, &extractions)?;
        ingest_pipeline_extractions(scoped, &extractions)?;

        if config.max_contexts > 0 {
            let _ = scoped.user.memory_index().trim_to_max(config.max_contexts);
            let _ = store.memory_index().trim_to_max(config.max_contexts);
        }
    }

    // Mark every candidate processed (including short conversations skipped by
    // `extract_batch` and sessions that returned empty memories) to avoid re-running LLM.
    mark_processed(store, &candidates)?;
    Ok(())
}

fn format_existing_memories(
    user: &MemoryStore,
    project: &MemoryStore,
    max_chars: usize,
) -> std::io::Result<String> {
    let user_md = user.read_memory().unwrap_or_default();
    let project_md = project.read_memory().unwrap_or_default();
    let combined = format!("### User memory\n{user_md}\n\n### Project memory\n{project_md}");
    if combined.len() <= max_chars {
        return Ok(combined);
    }
    Ok(format!(
        "{}…",
        crate::store::truncate_to_char_boundary(&combined, max_chars)
    ))
}

async fn find_changed_candidates(
    session_read: Arc<dyn EventReader>,
    store: &MemoryStore,
    current_session_id: &str,
    max_candidates: usize,
) -> Result<Vec<Candidate>, ExtensionError> {
    let summaries = session_read
        .list_session_summaries()
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;
    let processed = store
        .list_processed()
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    let mut candidates: Vec<Candidate> = summaries
        .into_iter()
        .filter_map(|s| {
            if s.session_id.as_ref() == current_session_id {
                return None;
            }
            if s.parent_session_id.is_some() {
                return None;
            }
            if s.source_extension.is_some() {
                return None;
            }
            if processed.get(s.session_id.as_ref()) == Some(&s.updated_at) {
                return None;
            }
            let updated = DateTime::parse_from_rfc3339(&s.updated_at).ok()?;
            Some(Candidate {
                summary: s,
                updated_at: updated.with_timezone(&Utc),
            })
        })
        .collect();

    candidates.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    candidates.truncate(max_candidates);
    Ok(candidates)
}

async fn extract_batch(
    session_read: Arc<dyn EventReader>,
    small_llm: &dyn LlmProvider,
    candidates: &[Candidate],
    existing_memories: &str,
    min_conversation_chars: usize,
) -> Result<Vec<(Candidate, Phase1Output)>, ExtensionError> {
    let mut blocks = Vec::new();
    let mut eligible = Vec::new();

    for candidate in candidates {
        let session_id = &candidate.summary.session_id;
        let read_model = session_read
            .session_read_model(session_id)
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        let conversation = extract_conversation(&read_model);
        if conversation.chars().count() < min_conversation_chars {
            continue;
        }

        blocks.push(format!(
            "### session_id: {}\n{conversation}",
            session_id.as_ref()
        ));
        eligible.push(candidate.clone());
    }

    if blocks.is_empty() {
        return Ok(Vec::new());
    }

    let current_date = Local::now().format("%Y-%m-%d").to_string();
    let user_prompt =
        prompts::batch_user_prompt(&blocks.join("\n\n"), &current_date, existing_memories);
    let messages = vec![
        LlmMessage {
            role: LlmRole::System,
            content: vec![LlmContent::Text {
                text: prompts::EXTRACT_SYSTEM.to_string(),
            }],
            name: None,
            reasoning_content: None,
        },
        LlmMessage {
            role: LlmRole::User,
            content: vec![LlmContent::Text { text: user_prompt }],
            name: None,
            reasoning_content: None,
        },
    ];

    let rx = small_llm
        .generate(messages, vec![])
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    let text = astrcode_extension_sdk::llm::collect_stream_text(rx)
        .await
        .unwrap_or_default();

    let batch = parse_batch_output(&text)?;
    map_batch_to_candidates(&eligible, batch)
}

fn parse_batch_output(text: &str) -> Result<BatchPhase1Output, ExtensionError> {
    let text = text.trim();
    let json_str = text
        .strip_prefix("```json")
        .and_then(|s| s.strip_suffix("```"))
        .map(|s| s.trim())
        .unwrap_or(text);
    serde_json::from_str(json_str)
        .map_err(|e| ExtensionError::Internal(format!("parse batch extraction: {e}")))
}

fn map_batch_to_candidates(
    candidates: &[Candidate],
    batch: BatchPhase1Output,
) -> Result<Vec<(Candidate, Phase1Output)>, ExtensionError> {
    let mut by_id: std::collections::BTreeMap<String, Vec<MemoryEntry>> =
        std::collections::BTreeMap::new();
    for session in batch.sessions {
        if session.memories.is_empty() {
            by_id.insert(session.session_id, Vec::new());
        } else {
            by_id.insert(session.session_id, session.memories);
        }
    }

    let mut results = Vec::new();
    for candidate in candidates {
        let sid = candidate.summary.session_id.as_ref();
        let memories = by_id.remove(sid).unwrap_or_default();
        results.push((candidate.clone(), Phase1Output { memories }));
    }
    Ok(results)
}

fn extract_conversation(model: &SessionReadModel) -> String {
    const MAX_BYTES: usize = 2000;
    const MAX_TURNS: usize = 15;

    let turns: Vec<String> = model
        .messages
        .iter()
        .filter_map(|msg| {
            let role = match msg.message.role {
                astrcode_extension_sdk::llm::LlmRole::User => "User",
                astrcode_extension_sdk::llm::LlmRole::Assistant => "Assistant",
                _ => return None,
            };
            let text = msg.message.joined_text("\n");
            if text.is_empty() {
                None
            } else {
                Some(format!("{role}: {text}"))
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

fn mark_processed(store: &MemoryStore, candidates: &[Candidate]) -> Result<(), ExtensionError> {
    let processed: Vec<ProcessedSession> = candidates
        .iter()
        .map(|c| ProcessedSession {
            session_id: c.summary.session_id.as_ref().to_string(),
            updated_at: c.summary.updated_at.clone(),
        })
        .collect();
    store
        .commit_pipeline_result(&processed, &[])
        .map_err(|e| ExtensionError::Internal(e.to_string()))
}

fn write_contexts(
    store: &MemoryStore,
    extractions: &[(Candidate, Phase1Output)],
) -> Result<(), ExtensionError> {
    let context_files: Vec<(String, String)> = extractions
        .iter()
        .filter(|(_, output)| {
            output
                .memories
                .iter()
                .any(|m| m.category != crate::scope::USER_CATEGORY)
        })
        .map(|(candidate, output)| {
            let memories: String = output
                .memories
                .iter()
                .filter(|m| m.category != crate::scope::USER_CATEGORY)
                .map(|m| format!("- [{}] {}", m.category, m.content))
                .collect::<Vec<_>>()
                .join("\n");
            let filename = format!("{}.md", candidate.summary.session_id.as_ref());
            let content = format!(
                "# Session {}\n\n## Extracted Memories\n{}",
                candidate.summary.session_id, memories
            );
            (filename, content)
        })
        .collect();

    if context_files.is_empty() {
        return Ok(());
    }

    store
        .commit_pipeline_result(&[], &context_files)
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    Ok(())
}

fn ingest_pipeline_extractions(
    scoped: &ScopedMemoryStores,
    extractions: &[(Candidate, Phase1Output)],
) -> Result<(), ExtensionError> {
    for (candidate, output) in extractions {
        if output.memories.is_empty() {
            continue;
        }
        let session_id = candidate.summary.session_id.as_ref();
        scoped
            .ingest_extracted_entries(
                &output.memories,
                crate::index::MemorySource::Pipeline,
                Some(session_id),
            )
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
    }
    Ok(())
}
