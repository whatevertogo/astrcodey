use std::{
    fs,
    path::{Path, PathBuf},
};

use astrcode_support::hostpaths::astrcode_dir;

use super::PromptFiles;

const MAX_IDENTITY_SIZE: usize = 8192;

pub async fn load_prompt_files(working_dir: &str, include_agents_rules: bool) -> PromptFiles {
    let working_dir = PathBuf::from(working_dir);
    let fallback_dir = working_dir.clone();
    tokio::task::spawn_blocking(move || read_prompt_files(&working_dir, include_agents_rules))
        .await
        .unwrap_or_else(|error| {
            tracing::warn!(error = %error, "prompt file preload task failed; reading inline");
            read_prompt_files(&fallback_dir, include_agents_rules)
        })
}

pub(super) fn read_prompt_files(working_dir: &Path, include_agents_rules: bool) -> PromptFiles {
    PromptFiles {
        identity: load_identity(&astrcode_dir().join("IDENTITY.md")),
        user_rules: if include_agents_rules {
            load_user_rules(&astrcode_dir().join("AGENTS.md"))
        } else {
            None
        },
        project_rules: if include_agents_rules {
            load_project_rules(working_dir)
        } else {
            None
        },
    }
}

fn load_identity(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return None;
    }

    let identity = if trimmed.len() > MAX_IDENTITY_SIZE {
        truncate_to_char_boundary(trimmed, MAX_IDENTITY_SIZE)
    } else {
        trimmed
    };
    Some(identity.to_string())
}

fn load_user_rules(path: &Path) -> Option<String> {
    let content = fs::read_to_string(path).ok()?;
    let content = content.trim();
    if content.is_empty() {
        return None;
    }

    Some(format!(
        "User-wide instructions from {}:\n{}",
        path.display(),
        content
    ))
}

fn load_project_rules(working_dir: &Path) -> Option<String> {
    let mut files = working_dir
        .ancestors()
        .map(|dir| dir.join("AGENTS.md"))
        .filter(|path| path.is_file())
        .collect::<Vec<_>>();
    files.reverse();
    if files.is_empty() {
        return None;
    }

    let mut content = String::from(
        "以下内容来自 AGENTS.md。必须遵守；如果规则冲突，目录更深的 AGENTS.md 优先。\n",
    );
    for path in files {
        if let Ok(text) = fs::read_to_string(&path) {
            content.push_str("\n--- ");
            content.push_str(&path.display().to_string());
            content.push_str(" ---\n");
            content.push_str(&text);
            if !text.ends_with('\n') {
                content.push('\n');
            }
        }
    }
    Some(content)
}

fn truncate_to_char_boundary(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }

    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
