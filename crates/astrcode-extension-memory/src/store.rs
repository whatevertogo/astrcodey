//! MemoryStore — MEMORY.md 文件读写与搜索。

use std::{io::Write, path::PathBuf, sync::Mutex};

use astrcode_support::hostpaths::{self, ensure_dir};

const MEMORY_FILE: &str = "MEMORY.md";
const HEADER: &str = "# Memory\n\n";
/// 2000 tokens ≈ 8000 字符（中文约 4000 字）。
const MAX_CHARS: usize = 8_000;

pub(crate) struct MemoryStore {
    dir: PathBuf,
    write_lock: Mutex<()>,
}

impl MemoryStore {
    pub(crate) fn new() -> Self {
        Self {
            dir: hostpaths::extensions_data_dir("astrcode.memory"),
            write_lock: Mutex::new(()),
        }
    }

    fn memory_path(&self) -> PathBuf {
        self.dir.join(MEMORY_FILE)
    }

    /// 确保目录和文件存在。延迟到第一次读写时调用。
    fn ensure_initialized(&self) -> std::io::Result<()> {
        ensure_dir(&self.dir)?;
        let path = self.memory_path();
        if !path.exists() {
            std::fs::write(&path, HEADER)?;
        }
        Ok(())
    }

    pub(crate) fn read_memory(&self) -> std::io::Result<String> {
        self.ensure_initialized()?;
        std::fs::read_to_string(self.memory_path())
    }

    pub(crate) fn append(&self, category: &str, content: &str) -> std::io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
        self.ensure_initialized()?;
        let sanitized = sanitize_entry(category, content);
        std::fs::OpenOptions::new()
            .append(true)
            .open(self.memory_path())?
            .write_all(sanitized.as_bytes())?;
        self.truncate_if_needed()?;
        Ok(())
    }

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

    /// 超出 MAX_CHARS 时截断：从头部删除最旧的条目，保留最新的。
    fn truncate_if_needed(&self) -> std::io::Result<()> {
        let content = self.read_memory()?;
        if content.len() <= MAX_CHARS {
            return Ok(());
        }

        let lines: Vec<&str> = content.lines().collect();
        let header_end = lines
            .iter()
            .position(|l| l.starts_with("<!--"))
            .unwrap_or(2);

        let mut blocks: Vec<Vec<&str>> = Vec::new();
        let mut i = header_end;
        while i < lines.len() {
            let mut block = Vec::new();
            if lines[i].starts_with("<!--") {
                block.push(lines[i]);
                i += 1;
            }
            while i < lines.len() && lines[i].starts_with("- ") {
                block.push(lines[i]);
                i += 1;
            }
            while i < lines.len() && lines[i].is_empty() {
                block.push(lines[i]);
                i += 1;
            }
            if !block.is_empty() {
                blocks.push(block);
            }
        }

        let header_lines = &lines[..header_end];
        while blocks.len() > 1 {
            blocks.remove(0);
            let total: usize = header_lines
                .iter()
                .chain(blocks.iter().flat_map(|b| b.iter()))
                .map(|l| l.len() + 1)
                .sum();
            if total <= MAX_CHARS {
                break;
            }
        }

        let mut output = header_lines.to_vec();
        for block in &blocks {
            output.extend(block);
        }
        std::fs::write(self.memory_path(), output.join("\n") + "\n")?;
        Ok(())
    }
}

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
