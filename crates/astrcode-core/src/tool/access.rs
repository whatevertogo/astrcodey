//! 工具访问外部资源的描述。
//!
//! 该描述供权限策略使用；并行调度只依据 [`super::ExecutionMode`]。

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
    /// 无法精确描述的副作用（如 shell）。
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
    let mut path = normalized.display().to_string().replace('\\', "/");
    while path.contains("//") {
        path = path.replace("//", "/");
    }
    if cfg!(windows) {
        path.make_ascii_lowercase();
    }
    path
}
