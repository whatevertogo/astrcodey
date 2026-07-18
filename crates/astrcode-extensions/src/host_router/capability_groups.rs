//! HostRouter 的能力域与窄后端依赖。

use std::sync::Arc;

use astrcode_core::{
    extension::{ExtensionCapability, OutboundNetworkService},
    llm::LlmProvider,
    storage::EventReader,
};

use super::{HostBackends, PublicHttpDispatcher, process::ProcessRunner};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum CapabilityGroupKind {
    Llm,
    Session,
    Context,
    Workspace,
    Process,
    Network,
    Extension,
}

impl CapabilityGroupKind {
    pub(super) fn for_capability(
        capability: &str,
        required_grant: Option<ExtensionCapability>,
    ) -> Option<Self> {
        match required_grant {
            Some(ExtensionCapability::MainModel | ExtensionCapability::SmallModel) => {
                Some(Self::Llm)
            },
            Some(
                ExtensionCapability::SessionControl
                | ExtensionCapability::SessionInspect
                | ExtensionCapability::SessionHistory,
            ) => Some(Self::Session),
            Some(ExtensionCapability::EmitEvents) => Some(Self::Context),
            Some(ExtensionCapability::WorkspaceRead | ExtensionCapability::WorkspaceWrite) => {
                Some(Self::Workspace)
            },
            Some(ExtensionCapability::ProcessSpawn) => Some(Self::Process),
            Some(ExtensionCapability::NetworkClient) => Some(Self::Network),
            Some(ExtensionCapability::PublicHttpDispatch) => Some(Self::Extension),
            None if capability.starts_with("astrcode.session.state.") => Some(Self::Context),
            _ => None,
        }
    }
}

pub(super) struct LlmCapabilityGroup {
    pub(super) main: Option<Arc<dyn LlmProvider>>,
    pub(super) small: Option<Arc<dyn LlmProvider>>,
}

pub(super) struct SessionCapabilityGroup {
    pub(super) reader: Option<Arc<dyn EventReader>>,
}

pub(super) struct WorkspaceCapabilityGroup {
    pub(super) default_working_dir: Option<String>,
}

pub(super) struct ProcessCapabilityGroup {
    pub(super) runner: Arc<ProcessRunner>,
    pub(super) default_working_dir: Option<String>,
}

pub(super) struct NetworkCapabilityGroup {
    pub(super) service: Option<Arc<dyn OutboundNetworkService>>,
}

#[derive(Default)]
pub(super) struct PublicHttpCapabilityGroup {
    pub(super) dispatcher: Option<Arc<dyn PublicHttpDispatcher>>,
}

pub(super) struct HostCapabilityGroups {
    pub(super) llm: LlmCapabilityGroup,
    pub(super) session: SessionCapabilityGroup,
    pub(super) workspace: WorkspaceCapabilityGroup,
    pub(super) process: ProcessCapabilityGroup,
    pub(super) network: NetworkCapabilityGroup,
    pub(super) public_http: PublicHttpCapabilityGroup,
}

impl From<HostBackends> for HostCapabilityGroups {
    fn from(backends: HostBackends) -> Self {
        Self {
            llm: LlmCapabilityGroup {
                main: backends.main_llm,
                small: backends.small_llm,
            },
            session: SessionCapabilityGroup {
                reader: backends.session_read,
            },
            workspace: WorkspaceCapabilityGroup {
                default_working_dir: backends.default_working_dir.clone(),
            },
            process: ProcessCapabilityGroup {
                runner: Arc::new(ProcessRunner::default()),
                default_working_dir: backends.default_working_dir,
            },
            network: NetworkCapabilityGroup {
                service: backends.outbound_network,
            },
            public_http: PublicHttpCapabilityGroup {
                dispatcher: backends.public_http_dispatcher,
            },
        }
    }
}
