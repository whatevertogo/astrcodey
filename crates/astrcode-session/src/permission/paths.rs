//! 从工具参数 JSON 提取可能的路径字符串。

use std::path::{Path, PathBuf};

/// 常见工具参数字段名。
const PATH_KEYS: &[&str] = &["path", "file", "filePath", "target", "directory", "dir"];

pub fn extract_tool_paths(input: &serde_json::Value) -> Vec<PathBuf> {
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
    if let Some(text) = value.as_str() {
        if !text.is_empty() {
            out.push(PathBuf::from(text));
        }
    }
}

/// 将路径转为相对 working_dir 的字符串用于 glob 匹配。
pub fn path_for_matching(path: &Path, working_dir: &Path) -> String {
    path.strip_prefix(working_dir)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_path_field() {
        let input = serde_json::json!({"path": "src/main.rs"});
        let paths = extract_tool_paths(&input);
        assert_eq!(paths, vec![PathBuf::from("src/main.rs")]);
    }
}
