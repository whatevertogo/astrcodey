//! Memory pipeline — SessionStart 时后台运行的两阶段提取管线。
//!
//! Phase 1: 从符合条件的历史会话中提取记忆（需要 small_llm）。
//! Phase 2: 将提取结果整合到 MEMORY.md + memory_summary.md。

use astrcode_core::{
    extension::{ExtensionError, SessionReadSource},
    llm::{LlmContent, LlmEvent, LlmMessage, LlmProvider},
    storage::{SessionReadModel, SessionSummary},
};
use chrono::{DateTime, Duration, Utc};

use crate::{
    pipeline_prompts,
    store::{MemoryStore, Phase1Output, Phase2Input, ProcessedSession},
};

// ─── Config ─────────────────────────────────────────────────────────────

struct PipelineConfig {
    max_candidates: usize,
    min_idle_minutes: i64,
}

impl Default for PipelineConfig {
    fn default() -> Self {
        Self {
            max_candidates: 5,
            min_idle_minutes: 30,
        }
    }
}

struct Candidate {
    summary: SessionSummary,
    updated_at: DateTime<Utc>,
}

// ─── Pipeline Entry ─────────────────────────────────────────────────────

pub async fn run(
    store: &MemoryStore,
    session_read: &dyn SessionReadSource,
    small_llm: Option<&dyn LlmProvider>,
    current_session_id: &str,
) -> Result<(), ExtensionError> {
    if store
        .finalize_pending_commit_if_exists()
        .map_err(|e| ExtensionError::Internal(e.to_string()))?
    {
        tracing::info!("memory pipeline finalized pending commit");
    }

    let config = PipelineConfig::default();

    // Phase 1: 有候选会话 + 有小模型 → 提取
    let candidates = find_candidates(session_read, store, current_session_id, &config).await?;
    if !candidates.is_empty() {
        if let Some(llm) = small_llm {
            extract(store, session_read, llm, &candidates).await?;
        }
        // small_llm == None：跳过提取，但仍可继续 Phase2
    }

    // Phase 2: 有未处理的 extractions → 整合
    let processed = store
        .list_processed()
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;
    let extraction_files: Vec<Phase2Input> = store
        .read_extractions()
        .map_err(|e| ExtensionError::Internal(e.to_string()))?
        .into_iter()
        .filter(|e| processed.get(&e.session_id) != Some(&e.updated_at))
        .collect();

    if extraction_files.is_empty() {
        return Ok(());
    }

    consolidate(store, small_llm, &extraction_files).await
}

// ─── Candidate Selection ────────────────────────────────────────────────

async fn find_candidates(
    session_read: &dyn SessionReadSource,
    store: &MemoryStore,
    current_session_id: &str,
    config: &PipelineConfig,
) -> Result<Vec<Candidate>, ExtensionError> {
    let summaries = session_read
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
            let updated_utc = updated.with_timezone(&Utc);
            // 空闲足够久 OR 有足够内容（兜底短会话）
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

    // 最近更新的优先
    candidates.sort_by_key(|c| std::cmp::Reverse(c.updated_at));
    candidates.truncate(config.max_candidates);

    Ok(candidates)
}

// ─── Phase 1: Extraction ────────────────────────────────────────────────

async fn extract(
    store: &MemoryStore,
    session_read: &dyn SessionReadSource,
    small_llm: &dyn LlmProvider,
    candidates: &[Candidate],
) -> Result<(), ExtensionError> {
    for candidate in candidates {
        let session_id = &candidate.summary.session_id;
        let read_model = session_read
            .read_session_model(session_id)
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        let conversation = extract_conversation(&read_model);
        if conversation.is_empty() {
            continue;
        }

        let prompt = pipeline_prompts::phase1_user_prompt(&conversation);
        let messages = vec![LlmMessage {
            role: astrcode_core::llm::LlmRole::User,
            content: vec![LlmContent::Text {
                text: format!("{}\n\n{}", pipeline_prompts::PHASE1_SYSTEM, prompt),
            }],
            name: None,
            reasoning_content: None,
        }];

        let rx = small_llm
            .generate(messages, vec![])
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;

        let text = collect_stream_text(rx).await;
        let output = parse_phase1_output(&text)?;

        let session_id_str = session_id.as_ref().to_string();
        store
            .write_extraction(&session_id_str, &candidate.summary.updated_at, &output)
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
    }
    Ok(())
}

fn extract_conversation(model: &SessionReadModel) -> String {
    let mut parts = Vec::new();
    let mut turn_count = 0u32;
    const MAX_TURNS: u32 = 20;
    const MAX_CHARS: usize = 4000;
    let mut total_chars = 0;

    for msg in &model.messages {
        if turn_count >= MAX_TURNS || total_chars >= MAX_CHARS {
            break;
        }
        let role = match msg.role {
            astrcode_core::llm::LlmRole::User => "User",
            astrcode_core::llm::LlmRole::Assistant => "Assistant",
            _ => continue,
        };
        let text: String = msg
            .content
            .iter()
            .filter_map(|c| match c {
                LlmContent::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        if text.is_empty() {
            continue;
        }
        parts.push(format!("{role}: {text}"));
        total_chars += text.len();
        if role == "Assistant" {
            turn_count += 1;
        }
    }

    parts.join("\n\n")
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

// ─── Phase 2: Consolidation ─────────────────────────────────────────────

async fn consolidate(
    store: &MemoryStore,
    small_llm: Option<&dyn LlmProvider>,
    extractions: &[Phase2Input],
) -> Result<(), ExtensionError> {
    let existing = store
        .read_memory()
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    let new_content = extractions
        .iter()
        .map(|e| {
            let memories: String = e
                .memories
                .iter()
                .map(|m| format!("- [{}] {}", m.category, m.content))
                .collect::<Vec<_>>()
                .join("\n");
            format!(
                "## Session {}\nSummary: {}\n{}",
                e.session_id, e.summary, memories
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n");

    let new_memory = if let Some(llm) = small_llm {
        consolidate_with_llm(llm, &existing, &new_content).await?
    } else {
        simple_merge(&existing, &new_content)
    };

    let summary = generate_summary(&new_memory, small_llm).await?;
    let processed: Vec<ProcessedSession> = extractions
        .iter()
        .map(|e| ProcessedSession {
            session_id: e.session_id.clone(),
            updated_at: e.updated_at.clone(),
        })
        .collect();
    let cleanup_ids: Vec<String> = extractions.iter().map(|e| e.session_id.clone()).collect();
    store
        .commit_consolidation(&new_memory, &summary, &processed, &cleanup_ids)
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    Ok(())
}

async fn consolidate_with_llm(
    llm: &dyn LlmProvider,
    existing: &str,
    extractions: &str,
) -> Result<String, ExtensionError> {
    let prompt = pipeline_prompts::phase2_user_prompt(existing, extractions);
    let messages = vec![LlmMessage {
        role: astrcode_core::llm::LlmRole::User,
        content: vec![LlmContent::Text {
            text: format!("{}\n\n{}", pipeline_prompts::PHASE2_SYSTEM, prompt),
        }],
        name: None,
        reasoning_content: None,
    }];

    let rx = llm
        .generate(messages, vec![])
        .await
        .map_err(|e| ExtensionError::Internal(e.to_string()))?;

    Ok(collect_stream_text(rx).await)
}

/// 无 small_llm 时的简单合并：按 category 分组追加。
fn simple_merge(existing: &str, new_content: &str) -> String {
    if existing.trim().is_empty() || existing.trim() == "# Memory" {
        format!("# Memory\n\n{new_content}\n")
    } else {
        format!("{existing}\n\n## New Extractions\n\n{new_content}\n")
    }
}

async fn generate_summary(
    memory: &str,
    small_llm: Option<&dyn LlmProvider>,
) -> Result<String, ExtensionError> {
    if let Some(llm) = small_llm {
        let prompt = pipeline_prompts::summary_prompt(memory);
        let messages = vec![LlmMessage {
            role: astrcode_core::llm::LlmRole::User,
            content: vec![LlmContent::Text { text: prompt }],
            name: None,
            reasoning_content: None,
        }];
        let rx = llm
            .generate(messages, vec![])
            .await
            .map_err(|e| ExtensionError::Internal(e.to_string()))?;
        let summary = collect_stream_text(rx).await;
        // 硬截断到 800 字符
        Ok(truncate_to_chars(&summary, 800))
    } else {
        // 无 LLM：取前 800 字符
        Ok(truncate_to_chars(memory, 800))
    }
}

pub(crate) fn truncate_to_chars(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    // 在字符边界处截断
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    // 尝试在行尾截断
    if let Some(pos) = s[..end].rfind('\n') {
        s[..pos].to_string()
    } else {
        s[..end].to_string()
    }
}

// ─── Stream Helper ──────────────────────────────────────────────────────

async fn collect_stream_text(mut rx: tokio::sync::mpsc::UnboundedReceiver<LlmEvent>) -> String {
    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => text.push_str(&delta),
            LlmEvent::Done { .. } => break,
            _ => {},
        }
    }
    text
}
