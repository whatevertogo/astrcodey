//! MemoryStore — MEMORY.md / contexts/ 文件读写。
//!
//! - **用户记忆**：`~/.astrcode/memory/`（`user_pref`，跨项目共享）
//! - **项目记忆**：`~/.astrcode/projects/<key>/extension_data/astrcode.memory/` （`project_ctx` /
//!   `decision` / `general`、contexts/、pipeline 状态）

use std::{collections::BTreeMap, path::PathBuf, sync::Arc, time::SystemTime};

use astrcode_support::hostpaths::{self, ensure_dir};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use crate::index::{MemoryIndex, MemorySource};

const MEMORY_FILE: &str = "MEMORY.md";
const CONTEXTS_DIR: &str = "contexts";
const PROCESSED_FILE: &str = "processed_sessions.json";
const HEADER: &str = "# Memory\n";

// ─── Section names (markdown headers → internal category keys) ──────

const USER_SECTIONS: &[(&str, &str)] = &[("user_pref", "## User Preferences")];

const PROJECT_SECTIONS: &[(&str, &str)] = &[
    ("project_ctx", "## Project Context"),
    ("decision", "## Decisions"),
    ("general", "## General"),
];

const VALID_CATEGORIES: &[&str] = &["user_pref", "project_ctx", "decision", "general"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MemoryStoreScope {
    User,
    Project,
}

impl MemoryStoreScope {
    fn sections(self) -> &'static [(&'static str, &'static str)] {
        match self {
            Self::User => USER_SECTIONS,
            Self::Project => PROJECT_SECTIONS,
        }
    }
}

fn category_to_header(scope: MemoryStoreScope, category: &str) -> &'static str {
    scope
        .sections()
        .iter()
        .find(|(key, _)| *key == category)
        .map(|(_, header)| *header)
        .unwrap_or(if scope == MemoryStoreScope::User {
            "## User Preferences"
        } else {
            "## General"
        })
}

fn header_to_category(scope: MemoryStoreScope, header: &str) -> Option<&'static str> {
    let trimmed = header.trim();
    scope
        .sections()
        .iter()
        .find(|(_, h)| *h == trimmed)
        .map(|(key, _)| *key)
}

// ─── Extraction Data Types (pipeline internal) ─────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Phase1Output {
    #[serde(default)]
    pub memories: Vec<MemoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryEntry {
    pub content: String,
    pub category: String,
    #[serde(default)]
    pub entities: Vec<String>,
    /// `add` (default), `update`, or `delete`.
    #[serde(default)]
    pub action: String,
    /// When updating/deleting, substring to match an existing memory line.
    #[serde(default)]
    pub replaces: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProcessedSession {
    pub session_id: String,
    pub updated_at: String,
}

// ─── Parsed MEMORY.md structure ─────────────────────────────────────

/// 解析后的 MEMORY.md 内容，按 section 分组。
struct ParsedMemory {
    /// 保留 header 行之前的所有行（如 `# Memory`）。
    preamble: Vec<String>,
    /// 按 section header 排序的条目。key 是 category（如 "user_pref"）。
    sections: Vec<(String, Vec<String>)>,
    scope: MemoryStoreScope,
}

impl ParsedMemory {
    fn parse(scope: MemoryStoreScope, content: &str) -> Self {
        let mut preamble = Vec::new();
        let mut sections: Vec<(String, Vec<String>)> = Vec::new();
        let mut current_category: Option<String> = None;
        let mut current_entries: Vec<String> = Vec::new();

        for line in content.lines() {
            if let Some(cat) = header_to_category(scope, line) {
                // 遇到新 section，先保存旧的
                if let Some(cat) = current_category.take() {
                    sections.push((cat, std::mem::take(&mut current_entries)));
                }
                current_category = Some(cat.to_string());
            } else if current_category.is_some() {
                // section 内的行，保留空行用于可读性但跳过纯空行
                let trimmed = line.trim();
                if !trimmed.is_empty() {
                    current_entries.push(trimmed.to_string());
                }
            } else {
                preamble.push(line.to_string());
            }
        }
        // 保存最后一个 section
        if let Some(cat) = current_category {
            sections.push((cat, current_entries));
        }

        // 确保所有 section 都存在（即使为空）
        for &(key, _) in scope.sections() {
            if !sections.iter().any(|(k, _)| k == key) {
                sections.push((key.to_string(), Vec::new()));
            }
        }
        sections.sort_by_key(|(key, _)| {
            scope
                .sections()
                .iter()
                .position(|(k, _)| k == key)
                .unwrap_or(usize::MAX)
        });

        ParsedMemory {
            preamble,
            sections,
            scope,
        }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.preamble {
            out.push_str(line);
            out.push('\n');
        }
        for (category, entries) in &self.sections {
            let header = category_to_header(self.scope, category);
            out.push('\n');
            out.push_str(header);
            out.push('\n');
            for entry in entries {
                out.push_str(entry);
                out.push('\n');
            }
        }
        out
    }

    fn add_entry(&mut self, category: &str, content: &str) {
        let sanitized = sanitize_content(content);
        let line = format!("- {sanitized}");
        if let Some(section) = self.sections.iter_mut().find(|(k, _)| k == category) {
            section.1.push(line);
        }
    }

    fn replace_or_add_entry(&mut self, category: &str, previous: &str, new_content: &str) -> bool {
        let prev_norm = crate::index::normalize_content(previous);
        let pattern_lower = previous.to_lowercase();
        for (cat, entries) in &mut self.sections {
            if cat != category {
                continue;
            }
            for entry in entries.iter_mut() {
                let line_body = entry.strip_prefix("- ").unwrap_or(entry.as_str());
                let line_norm = crate::index::normalize_content(line_body);
                if line_norm == prev_norm
                    || line_body.to_lowercase().contains(&pattern_lower)
                    || (prev_norm.len() >= 8
                        && (line_norm.contains(&prev_norm) || prev_norm.contains(&line_norm)))
                {
                    *entry = format!("- {}", sanitize_content(new_content));
                    return true;
                }
            }
        }
        self.add_entry(category, new_content);
        false
    }

    fn remove_entries_returning_content(&mut self, pattern: &str) -> Vec<String> {
        let pattern_lower = pattern.to_lowercase();
        let mut removed = Vec::new();
        for (category, entries) in &mut self.sections {
            let mut matched = Vec::new();
            entries.retain(|e| {
                if e.to_lowercase().contains(&pattern_lower) {
                    matched.push(format!("[{category}] {e}"));
                    false
                } else {
                    true
                }
            });
            removed.extend(matched);
        }
        removed
    }
}

// ─── MemoryStore ────────────────────────────────────────────────────

struct PreferenceLinesCache {
    memory_mtime: Option<SystemTime>,
    lines: Vec<String>,
}

pub(crate) struct MemoryStore {
    dir: PathBuf,
    write_lock: Mutex<()>,
    scope: MemoryStoreScope,
    preference_lines_cache: Mutex<Option<PreferenceLinesCache>>,
}

impl MemoryStore {
    pub(crate) fn memory_index(&self) -> MemoryIndex {
        MemoryIndex::new(&self.dir)
    }

    fn append_unlocked(&self, category: &str, content: &str) -> std::io::Result<()> {
        let safe_category = if VALID_CATEGORIES.contains(&category) {
            category
        } else {
            "general"
        };
        let mut parsed = self.read_parsed()?;
        parsed.add_entry(safe_category, content);
        atomic_write(&self.memory_path(), &parsed.render())
    }

    pub(crate) fn new_user() -> std::io::Result<Self> {
        let dir = hostpaths::memory_dir();
        ensure_dir(&dir)?;
        let store = Self {
            dir,
            write_lock: Mutex::new(()),
            scope: MemoryStoreScope::User,
            preference_lines_cache: Mutex::new(None),
        };
        if !store.memory_path().exists() {
            store.init_memory_file()?;
        }
        Ok(store)
    }

    pub(crate) fn new_project(working_dir: &str) -> std::io::Result<Self> {
        let project_key =
            astrcode_extension_sdk::types::project_key_from_path(std::path::Path::new(working_dir));
        let dir = hostpaths::astrcode_dir()
            .join("projects")
            .join(project_key)
            .join("extension_data")
            .join("astrcode.memory");
        ensure_dir(&dir)?;
        let store = Self {
            dir,
            write_lock: Mutex::new(()),
            scope: MemoryStoreScope::Project,
            preference_lines_cache: Mutex::new(None),
        };
        if !store.memory_path().exists() {
            store.init_memory_file()?;
        }
        Ok(store)
    }

    fn memory_path(&self) -> PathBuf {
        self.dir.join(MEMORY_FILE)
    }

    fn contexts_dir(&self) -> PathBuf {
        self.dir.join(CONTEXTS_DIR)
    }

    fn processed_path(&self) -> PathBuf {
        self.dir.join(PROCESSED_FILE)
    }

    /// 初始化 MEMORY.md，写入 header + 空 sections。
    fn init_memory_file(&self) -> std::io::Result<()> {
        let content = self.empty_memory_content();
        atomic_write(&self.memory_path(), &content)
    }

    fn empty_memory_content(&self) -> String {
        let mut out = String::from(HEADER);
        for &(_, header) in self.scope.sections() {
            out.push('\n');
            out.push_str(header);
            out.push('\n');
        }
        out
    }

    // ─── Read ──────────────────────────────────────────────────────

    pub(crate) fn read_memory(&self) -> std::io::Result<String> {
        std::fs::read_to_string(self.memory_path())
    }

    fn memory_file_mtime(&self) -> std::io::Result<Option<SystemTime>> {
        let path = self.memory_path();
        if !path.exists() {
            return Ok(None);
        }
        Ok(std::fs::metadata(path)?.modified().ok())
    }

    /// 返回 MEMORY.md 中 `user_pref` 类别的前几条，用于 PromptBuild 稳定注入。
    pub(crate) fn global_preference_lines(&self, limit: usize) -> std::io::Result<Vec<String>> {
        let mtime = self.memory_file_mtime()?;
        if let Some(cache) = self.preference_lines_cache.lock().as_ref() {
            if cache.memory_mtime == mtime && cache.lines.len() >= limit {
                return Ok(cache.lines.iter().take(limit).cloned().collect());
            }
        }

        let parsed = self.read_parsed()?;
        let mut lines = Vec::new();
        for (category, items) in &parsed.sections {
            if category != "user_pref" {
                continue;
            }
            for item in items {
                lines.push(item.clone());
            }
        }

        *self.preference_lines_cache.lock() = Some(PreferenceLinesCache {
            memory_mtime: mtime,
            lines: lines.clone(),
        });

        lines.truncate(limit);
        Ok(lines)
    }

    /// 读取并解析 MEMORY.md。
    fn read_parsed(&self) -> std::io::Result<ParsedMemory> {
        let content = self.read_memory()?;
        Ok(ParsedMemory::parse(self.scope, &content))
    }

    // ─── Write ─────────────────────────────────────────────────────

    /// 在指定 category section 追加一条记忆。
    pub(crate) fn append(&self, category: &str, content: &str) -> std::io::Result<()> {
        let _guard = self.write_lock.lock();
        self.append_unlocked(category, content)?;
        self.memory_index()
            .add_record(content, category, MemorySource::Manual, None, &[])?;
        Ok(())
    }

    /// 按内容子串匹配删除条目，返回被删除的条目列表。
    pub(crate) fn delete_by_content(&self, pattern: &str) -> std::io::Result<Vec<String>> {
        let _guard = self.write_lock.lock();
        let mut parsed = self.read_parsed()?;
        let removed = parsed.remove_entries_returning_content(pattern);
        if !removed.is_empty() {
            atomic_write(&self.memory_path(), &parsed.render())?;
        }
        let mut index_removed = self.memory_index().delete_by_content_match(pattern)?;
        if removed.is_empty() {
            Ok(index_removed)
        } else {
            index_removed.extend(removed);
            Ok(index_removed)
        }
    }

    // ─── List / Search (for /memory command) ───────────────────────

    pub(crate) fn list_entries(&self, limit: usize) -> std::io::Result<Vec<String>> {
        let index_entries = self.memory_index().list_display(limit)?;
        if !index_entries.is_empty() {
            return Ok(index_entries);
        }
        let parsed = self.read_parsed()?;
        let mut entries = Vec::new();
        for (category, items) in &parsed.sections {
            for item in items {
                entries.push(format!("[{category}] {item}"));
                if entries.len() >= limit {
                    return Ok(entries);
                }
            }
        }
        Ok(entries)
    }

    pub(crate) fn search(&self, query: &str, limit: usize) -> std::io::Result<Vec<String>> {
        let mut results = self.memory_index().search_substring(query, limit)?;
        if results.len() < limit {
            for line in self.memory_index().records_for_entity_boost(query)? {
                if !results.contains(&line) {
                    results.push(line);
                    if results.len() >= limit {
                        return Ok(results);
                    }
                }
            }
        }
        if results.len() >= limit {
            return Ok(results);
        }
        let parsed = self.read_parsed()?;
        let query_lower = query.to_lowercase();
        for (category, items) in &parsed.sections {
            for item in items {
                if item.to_lowercase().contains(&query_lower) {
                    let line = format!("[{category}] {item}");
                    if !results.iter().any(|r| r == &line) {
                        results.push(line);
                    }
                    if results.len() >= limit {
                        return Ok(results);
                    }
                }
            }
        }
        if results.len() < limit {
            for chunk in self.search_contexts(query, limit - results.len(), 600)? {
                let line = format!("[context] {chunk}");
                if !results.iter().any(|r| r == &line) {
                    results.push(line);
                    if results.len() >= limit {
                        break;
                    }
                }
            }
        }
        Ok(results)
    }

    // ─── Processed Sessions ────────────────────────────────────────

    /// 搜索 contexts/ 目录，返回与 query 相关的内容片段。
    ///
    /// BM25-like 评分：TF-IDF 加权 + 标题行加权 + 代码实体加权。
    pub(crate) fn search_contexts(
        &self,
        query: &str,
        max_results: usize,
        max_chars_per_file: usize,
    ) -> std::io::Result<Vec<String>> {
        let dir = self.contexts_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }

        let keywords: Vec<&str> = query.split_whitespace().filter(|w| w.len() >= 2).collect();
        if keywords.is_empty() {
            return Ok(Vec::new());
        }

        // 收集所有文档
        let docs: Vec<String> = std::fs::read_dir(&dir)?
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let path = entry.path();
                if path.extension().is_none_or(|e| e != "md") {
                    return None;
                }
                std::fs::read_to_string(path).ok()
            })
            .collect();

        let total_docs = docs.len();
        if total_docs == 0 {
            return Ok(Vec::new());
        }

        // 第一遍：统计每个关键词的文档频率
        let doc_freqs: Vec<usize> = keywords
            .iter()
            .map(|kw| {
                docs.iter()
                    .filter(|doc| doc.to_lowercase().contains(&kw.to_lowercase()))
                    .count()
            })
            .collect();

        // 第二遍：计算每个文档的 BM25 分数
        let mut scored: Vec<(f64, String)> = Vec::new();
        for content in &docs {
            let content_lower = content.to_lowercase();
            let headings: String = content
                .lines()
                .filter(|l| l.starts_with('#'))
                .collect::<Vec<_>>()
                .join(" ")
                .to_lowercase();

            let mut score: f64 = 0.0;
            for (i, kw) in keywords.iter().enumerate() {
                let kw_lower = kw.to_lowercase();
                let tf = (content_lower.matches(&kw_lower).count()).min(10) as f64;
                let df = doc_freqs[i] as f64;
                let idf = ((total_docs as f64 - df + 0.5) / (df + 0.5)).ln().max(0.0) + 1.0;

                let mut kw_score = tf * idf;

                // 标题行加权 1.5x
                if headings.contains(&kw_lower) {
                    kw_score *= 1.5;
                }

                // 代码实体加权 2.0x（路径、扩展名、CamelCase）
                if is_code_entity(kw) {
                    kw_score *= 2.0;
                }

                score += kw_score;
            }

            if score > 0.0 {
                let truncated = if content.len() > max_chars_per_file {
                    format!(
                        "{}…",
                        truncate_to_char_boundary(content, max_chars_per_file)
                    )
                } else {
                    content.clone()
                };
                scored.push((score, truncated));
            }
        }

        scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
        Ok(scored
            .into_iter()
            .take(max_results)
            .map(|(_, c)| c)
            .collect())
    }

    /// 清理超过 max_age_days 天的 contexts/ 文件，返回删除数量。
    pub(crate) fn cleanup_old_contexts(&self, max_age_days: u64) -> std::io::Result<usize> {
        let dir = self.contexts_dir();
        if !dir.exists() {
            return Ok(0);
        }

        let cutoff =
            std::time::SystemTime::now() - std::time::Duration::from_secs(max_age_days * 24 * 3600);
        let mut deleted = 0;

        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_none_or(|e| e != "md") {
                continue;
            }
            if let Ok(metadata) = path.metadata() {
                if let Ok(modified) = metadata.modified() {
                    if modified < cutoff {
                        let _ = std::fs::remove_file(&path);
                        deleted += 1;
                    }
                }
            }
        }

        Ok(deleted)
    }

    pub(crate) fn list_processed(&self) -> std::io::Result<BTreeMap<String, String>> {
        let path = self.processed_path();
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    /// Pipeline 写入 contexts/ 文件 + 更新 processed_sessions.json。
    pub(crate) fn commit_pipeline_result(
        &self,
        processed: &[ProcessedSession],
        context_files: &[(String, String)],
    ) -> std::io::Result<()> {
        let _guard = self.write_lock.lock();

        // 写入 context 文件
        if !context_files.is_empty() {
            let dir = self.contexts_dir();
            ensure_dir(&dir)?;
            for (filename, content) in context_files {
                atomic_write(&dir.join(filename), content)?;
            }
        }

        // 更新 processed_sessions.json
        let mut existing = BTreeMap::new();
        let path = self.processed_path();
        if path.exists() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                if let Ok(map) = serde_json::from_str::<BTreeMap<String, String>>(&content) {
                    existing = map;
                }
            }
        }
        for entry in processed {
            existing.insert(entry.session_id.clone(), entry.updated_at.clone());
        }
        let json = serde_json::to_string_pretty(&existing)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        atomic_write(&path, &json)?;

        Ok(())
    }

    /// Ingest extracted memories: similar entries update index + MEMORY.md; `delete` removes
    /// matches.
    pub(crate) fn ingest_extracted_entries(
        &self,
        entries: &[MemoryEntry],
        source: MemorySource,
        session_id: Option<&str>,
    ) -> std::io::Result<usize> {
        use crate::index::UpsertResult;

        let _guard = self.write_lock.lock();
        let index = self.memory_index();
        let mut changed = 0;
        for entry in entries {
            let action = entry.action.as_str();
            if action == "delete" {
                let pattern = entry
                    .replaces
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .unwrap_or(entry.content.as_str());
                if pattern.trim().is_empty() {
                    continue;
                }
                let removed_idx = index.delete_by_content_match(pattern)?;
                let removed_md = self.delete_by_content_unlocked(pattern)?;
                changed += removed_idx.len().max(removed_md.len());
                continue;
            }

            let category = if VALID_CATEGORIES.contains(&entry.category.as_str()) {
                entry.category.as_str()
            } else {
                "general"
            };

            match index.upsert_record(
                &entry.content,
                category,
                source.clone(),
                session_id,
                &entry.entities,
                entry.replaces.as_deref(),
            )? {
                UpsertResult::Unchanged => {},
                UpsertResult::Added(_) => {
                    self.append_unlocked(category, &entry.content)?;
                    changed += 1;
                },
                UpsertResult::Updated {
                    previous_content, ..
                } => {
                    let mut parsed = self.read_parsed()?;
                    parsed.replace_or_add_entry(category, &previous_content, &entry.content);
                    atomic_write(&self.memory_path(), &parsed.render())?;
                    changed += 1;
                },
            }
        }
        Ok(changed)
    }

    fn delete_by_content_unlocked(&self, pattern: &str) -> std::io::Result<Vec<String>> {
        let mut parsed = self.read_parsed()?;
        let removed = parsed.remove_entries_returning_content(pattern);
        if !removed.is_empty() {
            atomic_write(&self.memory_path(), &parsed.render())?;
        }
        let _ = self.memory_index().delete_by_content_match(pattern);
        Ok(removed)
    }
}

// ─── Helpers ────────────────────────────────────────────────────────

fn sanitize_content(content: &str) -> String {
    content
        .replace('\n', " ")
        .replace('\r', "")
        .trim()
        .to_string()
}

pub(crate) fn truncate_to_char_boundary(s: &str, max_bytes: usize) -> &str {
    if s.len() <= max_bytes {
        return s;
    }

    let mut end = max_bytes;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// 判断关键词是否像代码实体（路径、扩展名、CamelCase）。
fn is_code_entity(kw: &str) -> bool {
    kw.contains('/')
        || kw.contains('\\')
        || kw.contains('.')
        || kw.chars().filter(|c| c.is_uppercase()).count() > 0
}

/// 原子写入：先写 .tmp 再 rename，防止写到一半崩溃。
fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// ─── MemoryStorePool ───────────────────────────────────────────────────

const USER_STORE_KEY: &str = "__user_memory__";

/// 管理用户级 + 项目级 MemoryStore。
#[derive(Clone)]
pub(crate) struct MemoryStorePool {
    stores: Arc<Mutex<BTreeMap<String, Arc<MemoryStore>>>>,
}

impl MemoryStorePool {
    pub(crate) fn new() -> Self {
        Self {
            stores: Arc::new(Mutex::new(BTreeMap::new())),
        }
    }

    fn user_store(&self) -> std::io::Result<Arc<MemoryStore>> {
        let mut stores = self.stores.lock();
        if let Some(store) = stores.get(USER_STORE_KEY) {
            return Ok(store.clone());
        }
        let store = Arc::new(MemoryStore::new_user()?);
        stores.insert(USER_STORE_KEY.to_string(), store.clone());
        Ok(store)
    }

    /// 用户记忆（`~/.astrcode/memory/`）+ 当前项目记忆。
    pub(crate) fn get_scoped(
        &self,
        working_dir: &str,
    ) -> std::io::Result<crate::scope::ScopedMemoryStores> {
        let user = self.user_store()?;
        let mut stores = self.stores.lock();
        let project = if let Some(store) = stores.get(working_dir) {
            store.clone()
        } else {
            let store = Arc::new(MemoryStore::new_project(working_dir)?);
            stores.insert(working_dir.to_string(), store.clone());
            store
        };
        Ok(crate::scope::ScopedMemoryStores { user, project })
    }
}

impl Default for MemoryStorePool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_to_char_boundary;

    #[test]
    fn truncate_to_char_boundary_does_not_split_utf8() {
        assert_eq!(truncate_to_char_boundary("你好 world", 4), "你");
    }
}
