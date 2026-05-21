//! MemoryStore — MEMORY.md / memory_summary.md / extractions 文件读写。

use std::{collections::BTreeMap, io::Write, path::PathBuf};

use astrcode_support::hostpaths::{self, ensure_dir};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

const MEMORY_FILE: &str = "MEMORY.md";
const SUMMARY_FILE: &str = "memory_summary.md";
const EXTRACTIONS_DIR: &str = "extractions";
const PROCESSED_FILE: &str = "processed_sessions.json";
const PENDING_COMMIT_FILE: &str = "pending_commit.json";
const HEADER: &str = "# Memory\n\n";

// ─── Extraction Data Types ─────────────────────────────────────────────

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Phase1Output {
    pub summary: String,
    pub memories: Vec<MemoryEntry>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct MemoryEntry {
    pub content: String,
    pub category: String,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct Phase2Input {
    pub session_id: String,
    pub updated_at: String,
    pub summary: String,
    pub memories: Vec<MemoryEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct PendingCommit {
    memory: String,
    summary: String,
    processed: Vec<ProcessedSession>,
    cleanup_ids: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct ProcessedSession {
    pub session_id: String,
    pub updated_at: String,
}

// ─── MemoryStore ────────────────────────────────────────────────────────

pub(crate) struct MemoryStore {
    dir: PathBuf,
    write_lock: Mutex<()>,
}

impl MemoryStore {
    pub(crate) fn new() -> std::io::Result<Self> {
        let dir = hostpaths::extensions_data_dir("astrcode.memory");
        ensure_dir(&dir)?;
        let store = Self {
            dir,
            write_lock: Mutex::new(()),
        };
        let path = store.memory_path();
        if !path.exists() {
            std::fs::write(&path, HEADER)?;
        }
        Ok(store)
    }

    fn memory_path(&self) -> PathBuf {
        self.dir.join(MEMORY_FILE)
    }

    fn summary_path(&self) -> PathBuf {
        self.dir.join(SUMMARY_FILE)
    }

    fn extractions_dir(&self) -> PathBuf {
        self.dir.join(EXTRACTIONS_DIR)
    }

    fn processed_path(&self) -> PathBuf {
        self.dir.join(PROCESSED_FILE)
    }

    fn pending_commit_path(&self) -> PathBuf {
        self.dir.join(PENDING_COMMIT_FILE)
    }

    // ─── Memory.md Read/Write ──────────────────────────────────────────

    pub(crate) fn read_memory(&self) -> std::io::Result<String> {
        std::fs::read_to_string(self.memory_path())
    }

    pub(crate) fn append(&self, category: &str, content: &str) -> std::io::Result<()> {
        let _guard = self.write_lock.lock();
        let sanitized = sanitize_entry(category, content);
        std::fs::OpenOptions::new()
            .append(true)
            .open(self.memory_path())?
            .write_all(sanitized.as_bytes())?;
        Ok(())
    }

    // ─── Summary Read/Write ────────────────────────────────────────────

    pub(crate) fn read_summary(&self) -> std::io::Result<String> {
        let path = self.summary_path();
        if !path.exists() {
            return Ok(String::new());
        }
        std::fs::read_to_string(path)
    }

    // ─── Search / List ─────────────────────────────────────────────────

    pub(crate) fn search(&self, query: &str, limit: usize) -> std::io::Result<Vec<String>> {
        let content = self.read_memory()?;
        let query_lower = query.to_lowercase();
        let mut results = Vec::new();
        let mut current_category = "general".to_string();

        for line in content.lines() {
            if let Some(cat) = parse_category_line(line) {
                current_category = cat;
                continue;
            }
            if line.starts_with("- ") && line.to_lowercase().contains(&query_lower) {
                results.push(format!("[{current_category}] {line}"));
                if results.len() >= limit {
                    break;
                }
            }
        }
        Ok(results)
    }

    pub(crate) fn list_entries(&self, limit: usize) -> std::io::Result<Vec<String>> {
        let content = self.read_memory()?;
        let mut entries = Vec::new();
        let mut current_category = "general".to_string();

        for line in content.lines() {
            if let Some(cat) = parse_category_line(line) {
                current_category = cat;
                continue;
            }
            if line.starts_with("- ") {
                entries.push(format!("[{current_category}] {line}"));
                if entries.len() >= limit {
                    break;
                }
            }
        }
        Ok(entries)
    }

    // ─── Extractions (Phase1 intermediate results) ─────────────────────

    pub(crate) fn write_extraction(
        &self,
        session_id: &str,
        updated_at: &str,
        output: &Phase1Output,
    ) -> std::io::Result<()> {
        let _guard = self.write_lock.lock();
        let dir = self.extractions_dir();
        ensure_dir(&dir)?;
        let path = dir.join(format!("{session_id}.json"));
        let json = serde_json::to_string_pretty(&Phase2Input {
            session_id: session_id.to_string(),
            updated_at: updated_at.to_string(),
            summary: output.summary.clone(),
            memories: output.memories.clone(),
        })
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(path, json)
    }

    pub(crate) fn read_extractions(&self) -> std::io::Result<Vec<Phase2Input>> {
        let dir = self.extractions_dir();
        if !dir.exists() {
            return Ok(Vec::new());
        }
        let mut results = Vec::new();
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().is_some_and(|ext| ext == "json") {
                let session_id = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let content = std::fs::read_to_string(&path)?;
                if let Ok(extraction) = serde_json::from_str::<Phase2Input>(&content) {
                    results.push(extraction);
                } else if let Ok(phase1) = serde_json::from_str::<Phase1Output>(&content) {
                    results.push(Phase2Input {
                        session_id,
                        updated_at: String::new(),
                        summary: phase1.summary,
                        memories: phase1.memories,
                    });
                }
            }
        }
        results.sort_by(|a, b| a.session_id.cmp(&b.session_id));
        Ok(results)
    }

    // ─── Processed Sessions ────────────────────────────────────────────

    pub(crate) fn list_processed(&self) -> std::io::Result<BTreeMap<String, String>> {
        let path = self.processed_path();
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }

    pub(crate) fn commit_consolidation(
        &self,
        memory: &str,
        summary: &str,
        processed: &[ProcessedSession],
        cleanup_ids: &[String],
    ) -> std::io::Result<()> {
        let _guard = self.write_lock.lock();
        let commit = PendingCommit {
            memory: memory.to_string(),
            summary: summary.to_string(),
            processed: processed.to_vec(),
            cleanup_ids: cleanup_ids.to_vec(),
        };
        let json = serde_json::to_string_pretty(&commit)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        atomic_write(&self.pending_commit_path(), &json)?;
        self.apply_pending_commit_locked(&commit)?;
        std::fs::remove_file(self.pending_commit_path())?;
        Ok(())
    }

    pub(crate) fn finalize_pending_commit_if_exists(&self) -> std::io::Result<bool> {
        let _guard = self.write_lock.lock();
        let path = self.pending_commit_path();
        if !path.exists() {
            return Ok(false);
        }
        let content = std::fs::read_to_string(&path)?;
        let commit: PendingCommit = serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        self.apply_pending_commit_locked(&commit)?;
        std::fs::remove_file(path)?;
        Ok(true)
    }

    fn apply_pending_commit_locked(&self, commit: &PendingCommit) -> std::io::Result<()> {
        atomic_write(&self.memory_path(), &commit.memory)?;
        atomic_write(&self.summary_path(), &commit.summary)?;

        let mut processed = self.load_processed_locked()?;
        for entry in &commit.processed {
            processed.insert(entry.session_id.clone(), entry.updated_at.clone());
        }
        let processed_json = serde_json::to_string_pretty(&processed)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        atomic_write(&self.processed_path(), &processed_json)?;

        for id in &commit.cleanup_ids {
            let path = self.extractions_dir().join(format!("{id}.json"));
            if path.exists() {
                std::fs::remove_file(path)?;
            }
        }

        Ok(())
    }

    fn load_processed_locked(&self) -> std::io::Result<BTreeMap<String, String>> {
        let path = self.processed_path();
        if !path.exists() {
            return Ok(BTreeMap::new());
        }
        let content = std::fs::read_to_string(path)?;
        serde_json::from_str(&content)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    }
}

// ─── Helpers ────────────────────────────────────────────────────────────

fn sanitize_entry(category: &str, content: &str) -> String {
    let safe_category = if VALID_CATEGORIES.contains(&category) {
        category
    } else {
        "general"
    };
    let safe_content = content
        .replace('\n', " ")
        .replace('\r', "")
        .replace("-->", "→");
    format!("<!-- {safe_category} -->\n- {safe_content}\n\n")
}

const VALID_CATEGORIES: &[&str] = &["user_pref", "project_ctx", "decision", "general"];

fn parse_category_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.starts_with("<!--") && trimmed.ends_with("-->") {
        let inner = trimmed.strip_prefix("<!--")?.strip_suffix("-->")?.trim();
        if !inner.is_empty() {
            return Some(inner.to_string());
        }
    }
    None
}

/// 原子写入：先写 .tmp 再 rename，防止写到一半崩溃。
fn atomic_write(path: &std::path::Path, content: &str) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, content)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}
