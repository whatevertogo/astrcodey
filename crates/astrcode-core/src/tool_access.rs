//! 工具资源访问声明与冲突判定。
//!
//! 调度器用 [`ResourceAccess`] 判断两个工具调用能否并行执行：
//! 读/搜索互不冲突；写操作与路径重叠的任意操作冲突；[`ResourceAccess::All`] 与一切冲突。

use std::path::{Path, PathBuf};

/// 文件操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    Read,
    Search,
    Write,
    ReadWrite,
}

/// 单次工具调用声明的资源访问。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceAccess {
    File {
        operation: FileOperation,
        path: String,
        recursive: bool,
    },
    /// 无法精确描述的副作用（如 shell），与一切冲突。
    All,
}

impl ResourceAccess {
    pub fn read_file(path: impl AsRef<Path>) -> Self {
        Self::File {
            operation: FileOperation::Read,
            path: path_to_access_string(path.as_ref()),
            recursive: false,
        }
    }

    pub fn search_file(path: impl AsRef<Path>, recursive: bool) -> Self {
        Self::File {
            operation: FileOperation::Search,
            path: path_to_access_string(path.as_ref()),
            recursive,
        }
    }

    pub fn write_file(path: impl AsRef<Path>) -> Self {
        Self::file_write(path.as_ref(), false)
    }

    pub fn write_file_recursive(path: impl AsRef<Path>) -> Self {
        Self::file_write(path.as_ref(), true)
    }

    fn file_write(path: &Path, recursive: bool) -> Self {
        Self::File {
            operation: FileOperation::Write,
            path: path_to_access_string(path),
            recursive,
        }
    }

    pub fn read_write_file(path: impl AsRef<Path>) -> Self {
        Self::File {
            operation: FileOperation::ReadWrite,
            path: path_to_access_string(path.as_ref()),
            recursive: false,
        }
    }

    pub fn all() -> Self {
        Self::All
    }
}

/// 将路径转为用于冲突判定的词法字符串（不访问文件系统）。
fn path_to_access_string(path: &Path) -> String {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::ParentDir => {
                normalized.pop();
            },
            std::path::Component::CurDir => {},
            other => normalized.push(other),
        }
    }
    let mut s = normalized.display().to_string().replace('\\', "/");
    while s.contains("//") {
        s = s.replace("//", "/");
    }
    if cfg!(windows) {
        s = s.to_lowercase();
    }
    s
}

/// 两组资源访问是否存在冲突。
pub fn conflicts(left: &[ResourceAccess], right: &[ResourceAccess]) -> bool {
    left.iter()
        .any(|l| right.iter().any(|r| access_conflicts(l, r)))
}

fn access_conflicts(left: &ResourceAccess, right: &ResourceAccess) -> bool {
    match (left, right) {
        (ResourceAccess::All, _) | (_, ResourceAccess::All) => true,
        (
            ResourceAccess::File {
                operation: left_op,
                path: left_path,
                recursive: left_recursive,
            },
            ResourceAccess::File {
                operation: right_op,
                path: right_path,
                recursive: right_recursive,
            },
        ) => {
            let left_readonly = matches!(left_op, FileOperation::Read | FileOperation::Search);
            let right_readonly = matches!(right_op, FileOperation::Read | FileOperation::Search);
            if left_readonly && right_readonly {
                return false;
            }
            file_paths_overlap(left_path, *left_recursive, right_path, *right_recursive)
        },
    }
}

fn file_paths_overlap(
    left: &str,
    left_recursive: bool,
    right: &str,
    right_recursive: bool,
) -> bool {
    if left == right {
        return true;
    }
    if left_recursive && is_path_prefix(left, right) {
        return true;
    }
    if right_recursive && is_path_prefix(right, left) {
        return true;
    }
    false
}

fn is_path_prefix(prefix: &str, path: &str) -> bool {
    if prefix.is_empty() {
        return false;
    }
    if path == prefix {
        return true;
    }
    let prefix = prefix.trim_end_matches('/');
    if prefix.is_empty() {
        return path.starts_with('/');
    }
    path.starts_with(prefix)
        && (path.len() == prefix.len() || path.as_bytes().get(prefix.len()) == Some(&b'/'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_read_same_path_no_conflict() {
        let a = ResourceAccess::read_file("/src/main.rs");
        let b = ResourceAccess::read_file("/src/main.rs");
        assert!(!access_conflicts(&a, &b));
    }

    #[test]
    fn read_write_same_path_conflicts() {
        let a = ResourceAccess::read_file("/src/main.rs");
        let b = ResourceAccess::write_file("/src/main.rs");
        assert!(access_conflicts(&a, &b));
    }

    #[test]
    fn write_write_different_paths_no_conflict() {
        let a = ResourceAccess::write_file("/src/main.rs");
        let b = ResourceAccess::write_file("/src/lib.rs");
        assert!(!access_conflicts(&a, &b));
    }

    #[test]
    fn read_write_recursive_directory_conflicts() {
        let a = ResourceAccess::read_file("/src/main.rs");
        let b = ResourceAccess::write_file_recursive("/src");
        assert!(access_conflicts(&a, &b));
    }

    #[test]
    fn all_conflicts_with_read() {
        let a = ResourceAccess::all();
        let b = ResourceAccess::read_file("/src/main.rs");
        assert!(access_conflicts(&a, &b));
    }

    #[test]
    fn search_search_no_conflict() {
        let a = ResourceAccess::search_file("/src", true);
        let b = ResourceAccess::search_file("/lib", true);
        assert!(!access_conflicts(&a, &b));
    }

    #[test]
    fn path_normalization_treats_backslash_as_forward() {
        let a = ResourceAccess::write_file(r"\src\main.rs");
        let b = ResourceAccess::write_file("/src/main.rs");
        assert!(access_conflicts(&a, &b));
    }
}
