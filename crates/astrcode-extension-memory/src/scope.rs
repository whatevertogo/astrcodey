//! User vs project memory scopes — user/global under `~/.astrcode/memory/`, project under
//! extension_data.

use std::sync::Arc;

use crate::{
    index::MemorySource,
    store::{AppendResult, MemoryEntry, MemoryStore},
};

pub(crate) const USER_CATEGORY: &str = "user_pref";

fn push_unique_labeled(
    seen: &mut std::collections::HashSet<String>,
    out: &mut Vec<String>,
    prefix: &str,
    line: String,
) {
    let key = crate::index::normalize_content(&line);
    if key.is_empty() || !seen.insert(key) {
        return;
    }
    out.push(format!("[{prefix}] {line}"));
}

/// Project-scoped store plus the shared user memory store.
pub(crate) struct ScopedMemoryStores {
    pub user: Arc<MemoryStore>,
    pub project: Arc<MemoryStore>,
}

impl ScopedMemoryStores {
    pub(crate) fn store_for_category(&self, category: &str) -> &MemoryStore {
        if category == USER_CATEGORY {
            &self.user
        } else {
            &self.project
        }
    }

    pub(crate) fn append(&self, category: &str, content: &str) -> std::io::Result<AppendResult> {
        self.store_for_category(category).append(category, content)
    }

    pub(crate) fn delete_by_content(&self, pattern: &str) -> std::io::Result<Vec<String>> {
        let mut removed = self.user.delete_by_content(pattern)?;
        removed.extend(self.project.delete_by_content(pattern)?);
        Ok(removed)
    }

    pub(crate) fn global_preference_lines(&self, limit: usize) -> std::io::Result<Vec<String>> {
        self.user.global_preference_lines(limit)
    }

    pub(crate) fn list_entries(&self, limit: usize) -> std::io::Result<Vec<String>> {
        let half = limit.div_ceil(2);
        let mut out = Vec::new();
        for line in self.user.list_entries(half)? {
            out.push(format!("[user] {line}"));
            if out.len() >= limit {
                return Ok(out);
            }
        }
        for line in self.project.list_entries(half)? {
            out.push(format!("[project] {line}"));
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub(crate) fn search(&self, query: &str, limit: usize) -> std::io::Result<Vec<String>> {
        let half = limit.div_ceil(2);
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();

        for line in self.user.search(query, half)? {
            push_unique_labeled(&mut seen, &mut out, "user", line);
            if out.len() >= limit {
                return Ok(out);
            }
        }
        for line in self.project.search(query, half)? {
            push_unique_labeled(&mut seen, &mut out, "project", line);
            if out.len() >= limit {
                break;
            }
        }
        Ok(out)
    }

    pub(crate) fn ingest_extracted_entries(
        &self,
        entries: &[MemoryEntry],
        source: MemorySource,
        session_id: Option<&str>,
    ) -> std::io::Result<usize> {
        let mut user_entries = Vec::new();
        let mut project_entries = Vec::new();
        for entry in entries {
            if entry.category == USER_CATEGORY {
                user_entries.push(entry.clone());
            } else {
                project_entries.push(entry.clone());
            }
        }
        let mut added = 0;
        if !user_entries.is_empty() {
            added += self
                .user
                .ingest_extracted_entries(&user_entries, source.clone(), None)?;
        }
        if !project_entries.is_empty() {
            added += self
                .project
                .ingest_extracted_entries(&project_entries, source, session_id)?;
        }
        Ok(added)
    }
}
