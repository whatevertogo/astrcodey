//! MemoryStore — MEMORY.md / contexts/ 文件读写。
//!
//! MEMORY.md 使用干净的 markdown section 格式，按类别组织条目。
//! Pipeline 从历史会话提取的不相关上下文写入 contexts/ 目录。

use std::{
    collections::{BTreeMap, HashSet},
    path::PathBuf,
    sync::Arc,
};

use astrcode_support::{
    hash::fnv1a_hash_bytes,
    hostpaths::{self, ensure_dir},
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const MEMORY_FILE: &str = "MEMORY.md";
const CONTEXTS_DIR: &str = "contexts";
const PROCESSED_FILE: &str = "processed_sessions.json";
const HEADER: &str = "# Memory\n";

// ─── Section names (markdown headers → internal category keys) ──────

const SECTIONS: &[(&str, &str)] = &[
    ("user_pref", "## User Preferences"),
    ("project_ctx", "## Project Context"),
    ("decision", "## Decisions"),
    ("general", "## General"),
];

const VALID_CATEGORIES: &[&str] = &["user_pref", "project_ctx", "decision", "general"];

fn category_to_header(category: &str) -> &'static str {
    SECTIONS
        .iter()
        .find(|(key, _)| *key == category)
        .map(|(_, header)| *header)
        .unwrap_or("## General")
}

fn header_to_category(header: &str) -> Option<&'static str> {
    let trimmed = header.trim();
    SECTIONS
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
}

impl ParsedMemory {
    fn parse(content: &str) -> Self {
        let mut preamble = Vec::new();
        let mut sections: Vec<(String, Vec<String>)> = Vec::new();
        let mut current_category: Option<String> = None;
        let mut current_entries: Vec<String> = Vec::new();

        for line in content.lines() {
            if let Some(cat) = header_to_category(line) {
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
        for &(key, _) in SECTIONS {
            if !sections.iter().any(|(k, _)| k == key) {
                sections.push((key.to_string(), Vec::new()));
            }
        }
        // 按 SECTIONS 定义的顺序排列
        sections.sort_by_key(|(key, _)| {
            SECTIONS
                .iter()
                .position(|(k, _)| k == key)
                .unwrap_or(usize::MAX)
        });

        ParsedMemory { preamble, sections }
    }

    fn render(&self) -> String {
        let mut out = String::new();
        for line in &self.preamble {
            out.push_str(line);
            out.push('\n');
        }
        for (category, entries) in &self.sections {
            let header = category_to_header(category);
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

pub(crate) struct MemoryStore {
    dir: PathBuf,
    write_lock: Mutex<()>,
}

impl MemoryStore {
    pub(crate) fn new(project_path: Option<&str>) -> std::io::Result<Self> {
        let dir = if let Some(proj_path) = project_path {
            // 基于项目路径创建唯一的存储目录
            let project_key = astrcode_extension_sdk::types::project_key_from_path(
                std::path::Path::new(proj_path),
            );
            hostpaths::astrcode_dir()
                .join("projects")
                .join(project_key)
                .join("extension_data")
                .join("astrcode.memory")
        } else {
            // 全局存储（向后兼容）
            hostpaths::extensions_data_dir("astrcode.memory")
        };

        ensure_dir(&dir)?;
        let store = Self {
            dir,
            write_lock: Mutex::new(()),
        };
        let path = store.memory_path();
        if !path.exists() {
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
        let content = Self::empty_memory_content();
        atomic_write(&self.memory_path(), &content)
    }

    fn empty_memory_content() -> String {
        let mut out = String::from(HEADER);
        for &(_, header) in SECTIONS {
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

    /// 读取并解析 MEMORY.md。
    fn read_parsed(&self) -> std::io::Result<ParsedMemory> {
        let content = self.read_memory()?;
        Ok(ParsedMemory::parse(&content))
    }

    // ─── Write ─────────────────────────────────────────────────────

    /// 在指定 category section 追加一条记忆。
    pub(crate) fn append(&self, category: &str, content: &str) -> std::io::Result<()> {
        let _guard = self.write_lock.lock();
        let safe_category = if VALID_CATEGORIES.contains(&category) {
            category
        } else {
            "general"
        };
        let mut parsed = self.read_parsed()?;
        parsed.add_entry(safe_category, content);
        atomic_write(&self.memory_path(), &parsed.render())
    }

    /// 按内容子串匹配删除条目，返回被删除的条目列表。
    pub(crate) fn delete_by_content(&self, pattern: &str) -> std::io::Result<Vec<String>> {
        let _guard = self.write_lock.lock();
        let mut parsed = self.read_parsed()?;
        let removed = parsed.remove_entries_returning_content(pattern);
        if !removed.is_empty() {
            atomic_write(&self.memory_path(), &parsed.render())?;
        }
        Ok(removed)
    }

    // ─── List / Search (for /memory command) ───────────────────────

    pub(crate) fn list_entries(&self, limit: usize) -> std::io::Result<Vec<String>> {
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
        let parsed = self.read_parsed()?;
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        for (category, items) in &parsed.sections {
            for item in items {
                if item.to_lowercase().contains(&query_lower) {
                    results.push(format!("[{category}] {item}"));
                    if results.len() >= limit {
                        return Ok(results);
                    }
                }
            }
        }
        Ok(results)
    }

    // ─── Processed Sessions ────────────────────────────────────────

    /// 返回 MEMORY.md 中所有条目的 hash 集合（用于去重）。
    pub(crate) fn existing_entry_hashes(&self) -> std::io::Result<HashSet<u64>> {
        let parsed = self.read_parsed()?;
        let mut hashes = HashSet::new();
        for (_, entries) in &parsed.sections {
            for entry in entries {
                let normalized = normalize_for_hash(entry);
                hashes.insert(fnv1a_hash_bytes(normalized.as_bytes()));
            }
        }
        Ok(hashes)
    }

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
}

// ─── Helpers ────────────────────────────────────────────────────────

fn sanitize_content(content: &str) -> String {
    content
        .replace('\n', " ")
        .replace('\r', "")
        .trim()
        .to_string()
}

/// 标准化用于 hash 去重：lowercase + collapse whitespace。
fn normalize_for_hash(s: &str) -> String {
    s.trim()
        .strip_prefix("- ")
        .unwrap_or(s.trim())
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
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

/// 管理多个项目级 MemoryStore 的池。
///
/// 每个项目有独立的存储目录，通过 working_dir 获取对应的 store。
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

    /// 根据 working_dir 获取对应的 MemoryStore。
    ///
    /// 如果该 working_dir 的 store 不存在，则自动创建。
    pub(crate) fn get(&self, working_dir: &str) -> std::io::Result<Arc<MemoryStore>> {
        let mut stores = self.stores.lock();

        if let Some(store) = stores.get(working_dir) {
            return Ok(store.clone());
        }

        let store = Arc::new(MemoryStore::new(Some(working_dir))?);
        stores.insert(working_dir.to_string(), store.clone());
        Ok(store)
    }
}

impl Default for MemoryStorePool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::{normalize_for_hash, truncate_to_char_boundary};

    #[test]
    fn truncate_to_char_boundary_does_not_split_utf8() {
        assert_eq!(truncate_to_char_boundary("你好 world", 4), "你");
    }

    #[test]
    fn normalize_for_hash_ignores_markdown_bullet_prefix() {
        assert_eq!(
            normalize_for_hash("- The user prefers Rust"),
            normalize_for_hash("the user prefers rust")
        );
    }
}
