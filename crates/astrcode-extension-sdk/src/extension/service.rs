//! Versioned plugin services and host-attributed invocation context.
use std::{
    num::NonZeroU32,
    path::{Path, PathBuf},
    sync::Arc,
};

use serde_json::Value;

use super::{ExtensionCall, ExtensionCallContext};
use crate::{
    host::HostError,
    wire::service::{DependencyKindDto, ServiceDependencyDto},
};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ServiceKey {
    name: String,
    major: NonZeroU32,
}
impl ServiceKey {
    pub fn new(name: impl Into<String>, major: u32) -> Result<Self, String> {
        let name = name.into();
        let valid = !name.is_empty()
            && name.split('.').all(|part| {
                !part.is_empty()
                    && part
                        .bytes()
                        .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
            });
        if !valid {
            return Err(format!("invalid service name: {name}"));
        }
        let major = NonZeroU32::new(major)
            .ok_or_else(|| "service major version must be positive".to_owned())?;
        Ok(Self { name, major })
    }
}
impl std::str::FromStr for ServiceKey {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let (name, major) = value
            .rsplit_once('@')
            .ok_or_else(|| "service key must be name@major".to_owned())?;
        let version = major
            .parse::<u32>()
            .map_err(|_| "invalid service major version".to_owned())?;
        if version.to_string() != major {
            return Err("service major version must be canonical".into());
        }
        Self::new(name, version)
    }
}
impl std::fmt::Display for ServiceKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}@{}", self.name, self.major)
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyKind {
    Required,
    Optional,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceDependency {
    pub service: ServiceKey,
    pub kind: DependencyKind,
}

#[derive(Clone)]
pub struct ServiceContext {
    call: ExtensionCallContext,
    caller: String,
    working_dir: Option<PathBuf>,
    session_id: Option<String>,
}
impl ServiceContext {
    pub(crate) fn from_runtime(
        call: ExtensionCallContext,
        caller: String,
        working_dir: Option<PathBuf>,
        session_id: Option<String>,
    ) -> Self {
        Self {
            call,
            caller,
            working_dir,
            session_id,
        }
    }
    pub fn caller_extension_id(&self) -> &str {
        &self.caller
    }
    pub fn working_dir(&self) -> Option<&Path> {
        self.working_dir.as_deref()
    }
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }
}
impl ExtensionCall for ServiceContext {
    fn call(&self) -> &ExtensionCallContext {
        &self.call
    }
}
#[async_trait::async_trait]
pub trait ServiceHandler: Send + Sync {
    async fn invoke(&self, context: ServiceContext, input: Value) -> Result<Value, HostError>;
}
#[derive(Clone)]
pub struct ServiceRegistration {
    pub(crate) key: ServiceKey,
    pub(crate) handler: Arc<dyn ServiceHandler>,
}
impl ServiceRegistration {
    pub fn key(&self) -> &ServiceKey {
        &self.key
    }
    pub fn handler(&self) -> &Arc<dyn ServiceHandler> {
        &self.handler
    }
}

impl From<&ServiceDependency> for ServiceDependencyDto {
    fn from(d: &ServiceDependency) -> Self {
        Self {
            service: d.service.to_string(),
            kind: match d.kind {
                DependencyKind::Required => DependencyKindDto::Required,
                DependencyKind::Optional => DependencyKindDto::Optional,
            },
        }
    }
}
impl TryFrom<ServiceDependencyDto> for ServiceDependency {
    type Error = String;
    fn try_from(d: ServiceDependencyDto) -> Result<Self, String> {
        Ok(Self {
            service: d.service.parse::<ServiceKey>()?,
            kind: match d.kind {
                DependencyKindDto::Required => DependencyKind::Required,
                DependencyKindDto::Optional => DependencyKind::Optional,
            },
        })
    }
}
