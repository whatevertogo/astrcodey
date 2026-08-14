//! 工具调用的资源规划与执行租约。
//!
//! 工具负责把最终参数解释成 [`ToolPlan`]；session 基于 plan 做权限决策并签发
//! [`ResourceLease`]；真正的 Host 操作必须再次用 lease 校验实际访问。这里仅定义
//! 领域原语，不包含审批、锁或 Host 实现。

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

/// 文件操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOperation {
    Read,
    Search,
    Write,
    ReadWrite,
}

/// 非文件 Host 能力的最小资源域。
///
/// 这些值只表示本次调用获准接触的能力族。更细的请求约束仍由对应 Host DTO 与
/// capability policy 校验；文件访问则由 [`ResourceAccess::File`] 精确到路径。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostResource {
    Process,
    ToolResultArtifact,
    Session,
    Model,
    Network,
    Event,
    ExtensionHttp,
}

/// 单次工具调用声明的资源访问。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceAccess {
    File {
        operation: FileOperation,
        path: String,
        recursive: bool,
    },
    Host(HostResource),
    /// 不经过 Host、因而无法由 lease 细分或强制执行的外部副作用。
    Opaque,
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

    pub const fn host(resource: HostResource) -> Self {
        Self::Host(resource)
    }
}

/// 工具在一次调用中声明的完整资源集合。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ResourceSet(Vec<ResourceAccess>);

impl ResourceSet {
    pub fn new(resources: impl IntoIterator<Item = ResourceAccess>) -> Self {
        Self(resources.into_iter().collect())
    }

    pub fn as_slice(&self) -> &[ResourceAccess] {
        &self.0
    }

    pub fn iter(&self) -> impl Iterator<Item = &ResourceAccess> {
        self.0.iter()
    }

    pub fn into_vec(self) -> Vec<ResourceAccess> {
        self.0
    }
}

impl From<Vec<ResourceAccess>> for ResourceSet {
    fn from(resources: Vec<ResourceAccess>) -> Self {
        Self(resources)
    }
}

/// 工具对最终参数的纯资源规划结果。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ToolPlan {
    resources: ResourceSet,
}

impl ToolPlan {
    pub fn new(resources: impl Into<ResourceSet>) -> Self {
        Self {
            resources: resources.into(),
        }
    }

    pub fn opaque() -> Self {
        Self::from_resources([ResourceAccess::Opaque])
    }

    pub fn from_resources(resources: impl IntoIterator<Item = ResourceAccess>) -> Self {
        Self::new(ResourceSet::new(resources))
    }

    pub fn host(resource: HostResource) -> Self {
        Self::from_resources([ResourceAccess::host(resource)])
    }

    pub fn resources(&self) -> &ResourceSet {
        &self.resources
    }

    pub fn into_resources(self) -> ResourceSet {
        self.resources
    }
}

/// Host 为一次已经通过权限决策的工具调用签发的不可变资源租约。
///
/// lease 不可由 Extension 作者构造；它随工具调用上下文传到 HostRouter，并在调用结束时
/// 一并释放。当前类型只编码授权集合，资源互斥调度仍由 session 的执行模式负责。
#[derive(Debug, Clone)]
pub struct ResourceLease {
    resources: Arc<[ResourceAccess]>,
}

impl ResourceLease {
    pub fn from_plan(plan: &ToolPlan) -> Self {
        Self {
            resources: Arc::from(plan.resources().as_slice()),
        }
    }

    pub fn resources(&self) -> &[ResourceAccess] {
        &self.resources
    }

    pub fn permits(&self, required: &ResourceAccess) -> bool {
        self.resources
            .iter()
            .any(|granted| resource_covers(granted, required))
    }
}

fn resource_covers(granted: &ResourceAccess, required: &ResourceAccess) -> bool {
    match (granted, required) {
        (ResourceAccess::Opaque, ResourceAccess::Opaque) => true,
        (ResourceAccess::Host(granted), ResourceAccess::Host(required)) => granted == required,
        (
            ResourceAccess::File {
                operation: granted_operation,
                path: granted_path,
                recursive,
            },
            ResourceAccess::File {
                operation: required_operation,
                path: required_path,
                recursive: required_recursive,
            },
        ) => {
            file_operation_covers(*granted_operation, *required_operation)
                && path_covers(granted_path, *recursive, required_path, *required_recursive)
        },
        _ => false,
    }
}

fn file_operation_covers(granted: FileOperation, required: FileOperation) -> bool {
    match (granted, required) {
        (
            FileOperation::ReadWrite,
            FileOperation::Read | FileOperation::Write | FileOperation::ReadWrite,
        ) => true,
        _ => granted == required,
    }
}

fn path_covers(
    granted: &str,
    granted_recursive: bool,
    required: &str,
    required_recursive: bool,
) -> bool {
    if granted == required {
        return granted_recursive || !required_recursive;
    }
    granted_recursive && Path::new(required).starts_with(Path::new(granted))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_lease_enforces_domain_operation_and_recursive_path_boundaries() {
        let lease = ResourceLease::from_plan(&ToolPlan::new(ResourceSet::new([
            ResourceAccess::search_file("/workspace/src", true),
            ResourceAccess::read_write_file("/workspace/Cargo.toml"),
            ResourceAccess::host(HostResource::Process),
            ResourceAccess::host(HostResource::ToolResultArtifact),
        ])));

        let cases = [
            (
                ResourceAccess::search_file("/workspace/src/lib.rs", false),
                true,
            ),
            (ResourceAccess::search_file("/workspace/src", true), true),
            (ResourceAccess::search_file("/workspace/tests", true), false),
            (ResourceAccess::read_file("/workspace/Cargo.toml"), true),
            (ResourceAccess::write_file("/workspace/Cargo.toml"), true),
            (
                ResourceAccess::search_file("/workspace/Cargo.toml", false),
                false,
            ),
            (ResourceAccess::write_file("/workspace/src/lib.rs"), false),
            (ResourceAccess::host(HostResource::Process), true),
            (ResourceAccess::host(HostResource::ToolResultArtifact), true),
            (ResourceAccess::host(HostResource::Network), false),
        ];

        for (required, expected) in cases {
            assert_eq!(lease.permits(&required), expected, "{required:?}");
        }

        let non_recursive =
            ResourceLease::from_plan(&ToolPlan::from_resources([ResourceAccess::search_file(
                "/workspace/src",
                false,
            )]));
        assert!(!non_recursive.permits(&ResourceAccess::search_file("/workspace/src", true)));
        assert!(
            !non_recursive.permits(&ResourceAccess::search_file("/workspace/src/lib.rs", false,))
        );
    }
}
