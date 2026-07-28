use std::sync::Arc;

use astrcode_core::{llm::LlmProvider, tool::SessionOperations};
use tokio_util::sync::CancellationToken;

use crate::{
    extension::{
        ExtensionCapability, ExtensionConfig, ExtensionError, ExtensionEventSink, ExtensionTasks,
        OutboundNetworkService, Registrar, StopReason,
    },
    session_query::{SessionQuery, SessionQueryFactory},
};

#[async_trait::async_trait]
pub trait Extension: Send + Sync {
    fn id(&self) -> &str;

    fn capabilities(&self) -> &[ExtensionCapability] {
        &[]
    }

    fn register(&self, _registrar: &mut Registrar) {}

    async fn start(&self, _ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn stop(&self, _reason: StopReason) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn health(&self) -> Result<(), ExtensionError> {
        Ok(())
    }

    async fn on_config_changed(&self, _config: ExtensionConfig) -> Result<(), ExtensionError> {
        Ok(())
    }
}

#[derive(Clone)]
pub struct ExtensionCtx {
    tasks: ExtensionTasks,
    pub config: ExtensionConfig,
    startup_working_dir: Option<String>,
    event_sink: Option<Arc<dyn ExtensionEventSink>>,
    host_services: Option<Arc<ExtensionHostServices>>,
}

impl ExtensionCtx {
    pub fn with_host_services(
        tasks: ExtensionTasks,
        config: ExtensionConfig,
        startup_working_dir: Option<String>,
        event_sink: Option<Arc<dyn ExtensionEventSink>>,
        host_services: Option<Arc<ExtensionHostServices>>,
    ) -> Self {
        Self {
            tasks,
            config,
            startup_working_dir,
            event_sink,
            host_services,
        }
    }

    pub fn tasks(&self) -> &ExtensionTasks {
        &self.tasks
    }

    pub fn startup_working_dir(&self) -> Option<&str> {
        self.startup_working_dir.as_deref()
    }

    pub fn event_sink(&self) -> Option<&Arc<dyn ExtensionEventSink>> {
        self.event_sink.as_ref()
    }

    pub fn host_services(&self) -> Option<&Arc<ExtensionHostServices>> {
        self.host_services.as_ref()
    }

    pub fn shutdown(&self) -> CancellationToken {
        self.tasks.shutdown()
    }
}

pub struct ExtensionHostServices {
    session_queries: Option<Arc<dyn SessionQueryFactory>>,
    pub session_query: Option<Arc<dyn SessionQuery>>,
    pub main_llm: Option<Arc<dyn LlmProvider>>,
    pub small_llm: Option<Arc<dyn LlmProvider>>,
    pub session_ops: Option<Arc<dyn SessionOperations>>,
    pub outbound_network: Option<Arc<dyn OutboundNetworkService>>,
}

impl ExtensionHostServices {
    pub fn new(
        session_queries: Arc<dyn SessionQueryFactory>,
        main_llm: Option<Arc<dyn LlmProvider>>,
        small_llm: Option<Arc<dyn LlmProvider>>,
    ) -> Self {
        Self {
            session_queries: Some(session_queries),
            session_query: None,
            main_llm,
            small_llm,
            session_ops: None,
            outbound_network: None,
        }
    }

    pub fn with_session_ops(mut self, session_ops: Arc<dyn SessionOperations>) -> Self {
        self.session_ops = Some(session_ops);
        self
    }

    pub fn with_outbound_network(
        mut self,
        outbound_network: Arc<dyn OutboundNetworkService>,
    ) -> Self {
        self.outbound_network = Some(outbound_network);
        self
    }

    pub fn scoped_to(
        &self,
        extension_id: &str,
        capabilities: &[ExtensionCapability],
    ) -> Option<Self> {
        let session_query = capabilities
            .iter()
            .any(|capability| {
                matches!(
                    capability,
                    ExtensionCapability::SessionHistory | ExtensionCapability::SessionInspect
                )
            })
            .then(|| {
                self.session_queries
                    .as_ref()
                    .map(|queries| queries.for_extension(extension_id))
            })
            .flatten();
        let scoped = Self {
            session_queries: None,
            session_query,
            main_llm: capabilities
                .contains(&ExtensionCapability::MainModel)
                .then(|| self.main_llm.clone())
                .flatten(),
            small_llm: capabilities
                .contains(&ExtensionCapability::SmallModel)
                .then(|| self.small_llm.clone())
                .flatten(),
            session_ops: capabilities
                .contains(&ExtensionCapability::SessionControl)
                .then(|| self.session_ops.clone())
                .flatten(),
            outbound_network: capabilities
                .contains(&ExtensionCapability::NetworkClient)
                .then(|| self.outbound_network.clone())
                .flatten(),
        };
        (scoped.session_query.is_some()
            || scoped.main_llm.is_some()
            || scoped.small_llm.is_some()
            || scoped.session_ops.is_some()
            || scoped.outbound_network.is_some())
        .then_some(scoped)
    }
}
