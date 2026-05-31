//! Structured memory index (`memory_index.json`) for recall and dedup/upsert.

use std::{
    collections::{BTreeMap, HashSet},
    path::{Path, PathBuf},
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

pub(crate) const INDEX_FILE: &str = "memory_index.json";
pub(crate) const ENTITIES_FILE: &str = "entities_index.json";
const INDEX_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub(crate) enum MemorySource {
    Manual,
    Pipeline,
    #[serde(alias = "turn_end")]
    TurnEnd,
}

impl MemorySource {
    fn as_str(&self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Pipeline => "pipeline",
            Self::TurnEnd => "turn_end",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryRecord {
    pub id: String,
    pub content: String,
    pub category: String,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub created_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub entities: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct MemoryIndexFile {
    #[serde(default = "default_version")]
    version: u32,
    #[serde(default)]
    records: Vec<MemoryRecord>,
}

fn default_version() -> u32 {
    INDEX_VERSION
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct EntitiesIndexFile {
    #[serde(default)]
    links: BTreeMap<String, Vec<String>>,
}

pub(crate) fn normalize_content(s: &str) -> String {
    s.trim()
        .strip_prefix("- ")
        .unwrap_or(s.trim())
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn token_set(normalized: &str) -> HashSet<&str> {
    normalized
        .split_whitespace()
        .filter(|w| w.len() > 2)
        .collect()
}

fn word_jaccard(a: &str, b: &str) -> f64 {
    let sa = token_set(a);
    let sb = token_set(b);
    if sa.is_empty() || sb.is_empty() {
        return 0.0;
    }
    let inter = sa.intersection(&sb).count() as f64;
    let union = sa.union(&sb).count() as f64;
    inter / union
}

/// Find an existing record that should be updated instead of adding a duplicate.
pub(crate) fn find_similar_record_index(records: &[MemoryRecord], content: &str) -> Option<usize> {
    let norm_new = normalize_content(content);
    if norm_new.len() < 8 {
        return None;
    }

    let mut best: Option<(usize, f64)> = None;
    for (i, r) in records.iter().enumerate() {
        let norm_old = normalize_content(&r.content);
        if norm_new == norm_old {
            return Some(i);
        }
        let score = if norm_old.len() >= 12
            && (norm_new.contains(&norm_old) || norm_old.contains(&norm_new))
        {
            0.8
        } else {
            word_jaccard(&norm_new, &norm_old)
        };
        if score >= 0.55 && best.map(|(_, s)| score > s).unwrap_or(true) {
            best = Some((i, score));
        }
    }
    best.map(|(i, _)| i)
}

#[derive(Debug, Clone)]
pub(crate) enum UpsertResult {
    Added(MemoryRecord),
    Updated {
        record: MemoryRecord,
        previous_content: String,
    },
    Unchanged,
}

pub(crate) struct MemoryIndex {
    dir: PathBuf,
}

impl MemoryIndex {
    pub(crate) fn new(dir: &Path) -> Self {
        Self {
            dir: dir.to_path_buf(),
        }
    }

    fn index_path(&self) -> PathBuf {
        self.dir.join(INDEX_FILE)
    }

    fn entities_path(&self) -> PathBuf {
        self.dir.join(ENTITIES_FILE)
    }

    fn load_index(&self) -> std::io::Result<MemoryIndexFile> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(MemoryIndexFile::default());
        }
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    fn save_index(&self, index: &MemoryIndexFile) -> std::io::Result<()> {
        let path = self.index_path();
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_string_pretty(index)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn load_entities(&self) -> std::io::Result<EntitiesIndexFile> {
        let path = self.entities_path();
        if !path.exists() {
            return Ok(EntitiesIndexFile::default());
        }
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    fn save_entities(&self, entities: &EntitiesIndexFile) -> std::io::Result<()> {
        let path = self.entities_path();
        let tmp = path.with_extension("tmp");
        let json = serde_json::to_string_pretty(entities)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, json)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    fn link_entities(&self, record_id: &str, entities: &[String]) -> std::io::Result<()> {
        if entities.is_empty() {
            return Ok(());
        }
        let mut entity_index = self.load_entities()?;
        for entity in entities {
            let key = entity.to_lowercase();
            if key.len() < 2 {
                continue;
            }
            let ids = entity_index.links.entry(key).or_default();
            if !ids.iter().any(|id| id == record_id) {
                ids.push(record_id.to_string());
            }
        }
        self.save_entities(&entity_index)
    }

    fn find_by_substring(records: &[MemoryRecord], pattern: &str) -> Option<usize> {
        let pattern_lower = pattern.to_lowercase();
        records.iter().position(|r| {
            r.content.to_lowercase().contains(&pattern_lower)
                || normalize_content(&r.content).contains(&normalize_content(pattern))
        })
    }

    /// Insert or update: similar memories are refreshed; exact duplicate text is unchanged.
    pub(crate) fn upsert_record(
        &self,
        content: &str,
        category: &str,
        source: MemorySource,
        session_id: Option<&str>,
        entities: &[String],
        replaces: Option<&str>,
    ) -> std::io::Result<UpsertResult> {
        let content = content.trim();
        if content.is_empty() {
            return Ok(UpsertResult::Unchanged);
        }

        let mut index = self.load_index()?;
        let now = Utc::now().to_rfc3339();

        let similar_idx = replaces
            .and_then(|p| Self::find_by_substring(&index.records, p))
            .or_else(|| find_similar_record_index(&index.records, content));

        if let Some(idx) = similar_idx {
            let existing = &index.records[idx];
            if normalize_content(&existing.content) == normalize_content(content) {
                return Ok(UpsertResult::Unchanged);
            }
            let previous_content = existing.content.clone();
            let record_id = existing.id.clone();
            let created_at = existing.created_at.clone();
            index.records[idx] = MemoryRecord {
                id: record_id.clone(),
                content: content.to_string(),
                category: category.to_string(),
                source: source.as_str().to_string(),
                session_id: session_id
                    .map(str::to_string)
                    .or_else(|| existing.session_id.clone()),
                created_at,
                updated_at: Some(now),
                entities: if entities.is_empty() {
                    existing.entities.clone()
                } else {
                    entities.to_vec()
                },
            };
            let record = index.records[idx].clone();
            self.save_index(&index)?;
            self.link_entities(&record_id, entities)?;
            return Ok(UpsertResult::Updated {
                record,
                previous_content,
            });
        }

        let record = MemoryRecord {
            id: format!("mem_{}", Uuid::new_v4()),
            content: content.to_string(),
            category: category.to_string(),
            source: source.as_str().to_string(),
            session_id: session_id.map(str::to_string),
            created_at: now,
            updated_at: None,
            entities: entities.to_vec(),
        };

        let record_id = record.id.clone();
        index.records.push(record.clone());
        self.save_index(&index)?;
        self.link_entities(&record_id, entities)?;

        Ok(UpsertResult::Added(record))
    }

    /// Convenience wrapper around `upsert_record` for manual append paths.
    ///
    /// May fuzzy-match and update an existing similar record instead of always
    /// inserting a new one. Callers that need strictly append-only behavior must
    /// avoid content that dedupes with existing records.
    pub(crate) fn add_record(
        &self,
        content: &str,
        category: &str,
        source: MemorySource,
        session_id: Option<&str>,
        entities: &[String],
    ) -> std::io::Result<Option<MemoryRecord>> {
        match self.upsert_record(content, category, source, session_id, entities, None)? {
            UpsertResult::Added(r) => Ok(Some(r)),
            UpsertResult::Updated { record, .. } => Ok(Some(record)),
            UpsertResult::Unchanged => Ok(None),
        }
    }

    pub(crate) fn delete_by_content_match(&self, pattern: &str) -> std::io::Result<Vec<String>> {
        let pattern_lower = pattern.to_lowercase();
        let mut index = self.load_index()?;
        let mut removed = Vec::new();
        let mut removed_ids = HashSet::new();

        index.records.retain(|r| {
            if r.content.to_lowercase().contains(&pattern_lower) {
                removed.push(format!("[{}] - {}", r.category, r.content));
                removed_ids.insert(r.id.clone());
                false
            } else {
                true
            }
        });

        if removed.is_empty() {
            return Ok(removed);
        }

        self.save_index(&index)?;

        let mut entities = self.load_entities()?;
        for ids in entities.links.values_mut() {
            ids.retain(|id| !removed_ids.contains(id));
        }
        entities.links.retain(|_, ids| !ids.is_empty());
        self.save_entities(&entities)?;

        Ok(removed)
    }

    pub(crate) fn search_substring(
        &self,
        query: &str,
        limit: usize,
    ) -> std::io::Result<Vec<String>> {
        let query_lower = query.to_lowercase();
        let index = self.load_index()?;
        let mut results = Vec::new();
        for r in &index.records {
            if r.content.to_lowercase().contains(&query_lower) {
                results.push(format!("[{}] {}", r.category, r.content));
                if results.len() >= limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub(crate) fn list_display(&self, limit: usize) -> std::io::Result<Vec<String>> {
        let index = self.load_index()?;
        Ok(index
            .records
            .iter()
            .rev()
            .take(limit)
            .map(|r| format!("[{}] - {}", r.category, r.content))
            .collect())
    }

    /// Rank index records by relevance to `query` (project memories only).
    pub(crate) fn rank_for_query(
        &self,
        query: &str,
        limit: usize,
        min_score: f64,
    ) -> std::io::Result<Vec<(f64, String)>> {
        let query = query.trim();
        if query.len() < 4 {
            return Ok(Vec::new());
        }

        let keywords: Vec<String> = query
            .split_whitespace()
            .filter(|w| w.chars().count() >= 2)
            .map(|w| w.to_lowercase())
            .collect();
        if keywords.is_empty() {
            return Ok(Vec::new());
        }

        let index = self.load_index()?;
        let entities = self.load_entities()?;
        let query_lower = query.to_lowercase();
        let norm_query = normalize_content(query);

        let mut entity_boost_ids = HashSet::new();
        for (entity, mem_ids) in &entities.links {
            if query_lower.contains(entity) {
                entity_boost_ids.extend(mem_ids.iter().cloned());
            }
        }

        let mut scored = Vec::new();
        for r in &index.records {
            if r.category == crate::scope::USER_CATEGORY {
                continue;
            }
            let norm = normalize_content(&r.content);
            let mut score = word_jaccard(&norm_query, &norm);
            let content_lower = r.content.to_lowercase();
            for kw in &keywords {
                if content_lower.contains(kw) {
                    score += 0.12;
                }
            }
            if entity_boost_ids.contains(&r.id) {
                score += 0.25;
            }
            if score >= min_score {
                scored.push((score, format!("[{}] {}", r.category, r.content)));
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(limit);
        Ok(scored)
    }

    pub(crate) fn records_for_entity_boost(&self, query: &str) -> std::io::Result<Vec<String>> {
        let entities = self.load_entities()?;
        let query_lower = query.to_lowercase();
        let index = self.load_index()?;
        let mut ids = HashSet::new();
        for (entity, mem_ids) in &entities.links {
            if query_lower.contains(entity) {
                ids.extend(mem_ids.iter().cloned());
            }
        }
        Ok(index
            .records
            .iter()
            .filter(|r| ids.contains(&r.id))
            .map(|r| format!("[{}] {}", r.category, r.content))
            .collect())
    }

    /// Returns content of records similar to the given content, without modifying anything.
    pub(crate) fn find_similar(&self, content: &str) -> std::io::Result<Vec<String>> {
        let norm_new = normalize_content(content);
        if norm_new.len() < 8 {
            return Ok(Vec::new());
        }
        let index = self.load_index()?;
        let mut similar = Vec::new();
        for r in &index.records {
            let norm_old = normalize_content(&r.content);
            if norm_new == norm_old {
                similar.push(r.content.clone());
                continue;
            }
            let score = if norm_old.len() >= 12
                && (norm_new.contains(&norm_old) || norm_old.contains(&norm_new))
            {
                0.8
            } else {
                word_jaccard(&norm_new, &norm_old)
            };
            if score >= 0.55 {
                similar.push(r.content.clone());
            }
        }
        Ok(similar)
    }

    pub(crate) fn trim_to_max(&self, max_records: usize) -> std::io::Result<usize> {
        let mut index = self.load_index()?;
        if index.records.len() <= max_records {
            return Ok(0);
        }
        let remove_count = index.records.len() - max_records;
        index.records.drain(0..remove_count);
        self.save_index(&index)?;
        Ok(remove_count)
    }
}

#[cfg(test)]
mod tests {
    use astrcode_support::hostpaths::ensure_dir;
    use tempfile::TempDir;

    use super::*;

    #[test]
    fn upsert_updates_similar_memories() {
        let tmp = TempDir::new().unwrap();
        ensure_dir(tmp.path()).unwrap();
        let index = MemoryIndex::new(tmp.path());

        assert!(matches!(
            index
                .upsert_record(
                    "User prefers Rust for backend services",
                    "user_pref",
                    MemorySource::Pipeline,
                    None,
                    &[],
                    None,
                )
                .unwrap(),
            UpsertResult::Added(_)
        ));
        assert!(matches!(
            index
                .upsert_record(
                    "User prefers Rust for all backend work",
                    "user_pref",
                    MemorySource::Pipeline,
                    None,
                    &[],
                    None,
                )
                .unwrap(),
            UpsertResult::Updated { .. }
        ));
        let listed = index.list_display(10).unwrap();
        assert_eq!(listed.len(), 1);
        assert!(listed[0].contains("all backend"));
    }

    #[test]
    fn link_entities_dedupes_record_ids_on_update() {
        let tmp = TempDir::new().unwrap();
        ensure_dir(tmp.path()).unwrap();
        let index = MemoryIndex::new(tmp.path());

        index
            .upsert_record(
                "User prefers Rust for backend",
                "user_pref",
                MemorySource::Manual,
                None,
                &["rust".to_string()],
                None,
            )
            .unwrap();
        index
            .upsert_record(
                "User prefers Rust for all backend work",
                "user_pref",
                MemorySource::Manual,
                None,
                &["rust".to_string()],
                None,
            )
            .unwrap();

        let entities_path = tmp.path().join(ENTITIES_FILE);
        let content = std::fs::read_to_string(entities_path).unwrap();
        let entities: EntitiesIndexFile = serde_json::from_str(&content).unwrap();
        let ids = entities.links.get("rust").expect("rust entity linked");
        assert_eq!(ids.len(), 1, "entity index must not duplicate record ids");
    }

    #[test]
    fn upsert_skips_exact_duplicate() {
        let tmp = TempDir::new().unwrap();
        ensure_dir(tmp.path()).unwrap();
        let index = MemoryIndex::new(tmp.path());

        index
            .upsert_record(
                "User prefers Rust",
                "user_pref",
                MemorySource::Manual,
                None,
                &[],
                None,
            )
            .unwrap();
        assert!(matches!(
            index
                .upsert_record(
                    "- user prefers rust",
                    "user_pref",
                    MemorySource::Manual,
                    None,
                    &[],
                    None,
                )
                .unwrap(),
            UpsertResult::Unchanged
        ));
    }
}
