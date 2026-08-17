//! 从工具参数 JSON 提取可能的路径字符串。

use std::path::{Path, PathBuf};

use globset::GlobSet;

/// 常见工具参数字段名。
const PATH_KEYS: &[&str] = &["path", "file", "filePath", "target", "directory", "dir"];

pub(super) fn extract_tool_paths(input: &serde_json::Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    collect_paths(input, &mut paths);
    paths.sort();
    paths.dedup();
    paths
}

fn collect_paths(value: &serde_json::Value, out: &mut Vec<PathBuf>) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, val) in map {
                if PATH_KEYS.contains(&key.as_str()) {
                    push_path_value(val, out);
                } else if key == "paths" || key == "files" {
                    if let serde_json::Value::Array(items) = val {
                        for item in items {
                            push_path_value(item, out);
                        }
                    }
                } else {
                    collect_paths(val, out);
                }
            }
        },
        serde_json::Value::Array(items) => {
            for item in items {
                collect_paths(item, out);
            }
        },
        _ => {},
    }
}

fn push_path_value(value: &serde_json::Value, out: &mut Vec<PathBuf>) {
    if let Some(text) = value.as_str()
        && !text.is_empty()
    {
        out.push(PathBuf::from(text));
    }
}

/// 将路径转为相对 working_dir 的字符串用于 glob 匹配。
pub(super) fn path_for_matching(path: &Path, working_dir: &Path) -> String {
    path.strip_prefix(working_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// 路径命中 glob 的两种形式：相对 working_dir 的 rel，或原始路径字符串。
///
/// configured.rs 与 sensitive_file_ask.rs 共用同一匹配语义；git_path_ask 刻意不用
/// glob（对 rel 做字符串前缀/包含匹配），语义不同，故不收敛到这里。
pub(super) fn path_matches_glob(path: &Path, working_dir: &Path, globset: &GlobSet) -> bool {
    let rel = path_for_matching(path, working_dir);
    globset.is_match(&rel) || globset.is_match(path.to_string_lossy().as_ref())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::extract_tool_paths;

    #[test]
    fn extracts_nonempty_path_fields() {
        assert_eq!(
            extract_tool_paths(&serde_json::json!({"path": "src/main.rs"})),
            vec![PathBuf::from("src/main.rs")]
        );
    }
}
