use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use async_trait::async_trait;
use serde_json::Value;

use super::CallContextBuilder;
use crate::{
    extension::{
        Extension, ExtensionConfig, ExtensionError, ExtensionManifest, ExtensionRegistrations,
        ExtensionStartContext, ExtensionStopContext, ExtensionTasks, Registrar, RegistrationError,
        StopReason,
    },
    host::{
        ExtensionHost, HostError, HostOperation,
        internal::{HostInvoker, HostScope, extension_host},
    },
};

/// One invocation captured by [`MockExtensionHost`].
#[derive(Debug, Clone, PartialEq)]
pub struct MockHostInvocation {
    pub operation: HostOperation,
    pub input: Value,
}

#[derive(Default)]
struct MockHostState {
    responses: HashMap<HostOperation, Result<Value, HostError>>,
    invocations: Vec<MockHostInvocation>,
}

/// Explicitly scoped host fixture for extension author tests.
///
/// A configured response makes that operation's backend available. Capability and context checks
/// still run through the real [`ExtensionHost`] preflight path, so permission, backend, and scope
/// failures remain distinguishable in tests.
#[derive(Clone, Default)]
pub struct MockExtensionHost {
    state: Arc<Mutex<MockHostState>>,
    grants: Vec<crate::extension::ExtensionCapability>,
    session_context_available: bool,
    workspace_context_available: bool,
}

impl MockExtensionHost {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn grant(mut self, capability: crate::extension::ExtensionCapability) -> Self {
        if !self.grants.contains(&capability) {
            self.grants.push(capability);
        }
        self
    }

    pub fn session_context(mut self, available: bool) -> Self {
        self.session_context_available = available;
        self
    }

    pub fn workspace_context(mut self, available: bool) -> Self {
        self.workspace_context_available = available;
        self
    }

    pub fn respond(self, operation: HostOperation, output: Value) -> Self {
        self.with_response(operation, Ok(output))
    }

    pub fn fail(self, operation: HostOperation, error: HostError) -> Self {
        self.with_response(operation, Err(error))
    }

    fn with_response(self, operation: HostOperation, response: Result<Value, HostError>) -> Self {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .responses
            .insert(operation, response);
        self
    }

    pub fn host(&self) -> ExtensionHost {
        let available = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .responses
            .keys()
            .copied()
            .collect::<Vec<_>>();
        extension_host(
            Arc::new(MockHostInvoker {
                state: Arc::clone(&self.state),
            }),
            HostScope::new(
                self.grants.iter().copied(),
                available,
                self.session_context_available,
                self.workspace_context_available,
            ),
        )
    }

    pub fn invocations(&self) -> Vec<MockHostInvocation> {
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .invocations
            .clone()
    }
}

struct MockHostInvoker {
    state: Arc<Mutex<MockHostState>>,
}

#[async_trait]
impl HostInvoker for MockHostInvoker {
    async fn invoke(&self, operation: HostOperation, input: Value) -> Result<Value, HostError> {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state
            .invocations
            .push(MockHostInvocation { operation, input });
        state.responses.get(&operation).cloned().unwrap_or_else(|| {
            Err(HostError::new(
                crate::WireErrorCode::BackendUnavailable,
                format!("{} has no configured mock response", operation.wire_name()),
            ))
        })
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
}

/// Manifest and immutable registrations produced by the real authoring boundary.
pub struct RegisteredExtension {
    manifest: ExtensionManifest,
    registrations: ExtensionRegistrations,
}

impl RegisteredExtension {
    pub fn manifest(&self) -> &ExtensionManifest {
        &self.manifest
    }

    pub fn registrations(&self) -> &ExtensionRegistrations {
        &self.registrations
    }

    pub fn into_parts(self) -> (ExtensionManifest, ExtensionRegistrations) {
        (self.manifest, self.registrations)
    }
}

/// Runs an extension's real manifest, registration, and local capability validation path.
pub struct RegistrationHarness;

impl RegistrationHarness {
    pub fn register(extension: &dyn Extension) -> Result<RegisteredExtension, RegistrationError> {
        let manifest = extension.manifest();
        let mut registrar = Registrar::new();
        extension.register(&mut registrar);
        let (manifest, registrations) = registrar.finish(manifest)?;
        Ok(RegisteredExtension {
            manifest,
            registrations,
        })
    }
}

/// Observable operations performed by [`ExtensionLifecycleHarness`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleHarnessEvent {
    Start,
    TasksActivated,
    TasksCancelled,
    TasksDrained,
    Stop(StopReason),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleState {
    Registered,
    Started,
    Stopped,
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleHarnessError {
    #[error("invalid lifecycle transition: {0}")]
    InvalidTransition(&'static str),
    #[error(transparent)]
    Extension(#[from] ExtensionError),
    #[error("extension tasks did not drain")]
    TasksDidNotDrain,
    #[error("start failed: {start}; startup rollback stop also failed: {stop}")]
    StartupRollback { start: String, stop: String },
}

/// Small lifecycle runtime for verifying start/tasks/stop ordering in extension tests.
pub struct ExtensionLifecycleHarness {
    extension: Arc<dyn Extension>,
    registered: RegisteredExtension,
    tasks: ExtensionTasks,
    state: LifecycleState,
    events: Vec<LifecycleHarnessEvent>,
    config: Value,
    startup_working_dir: Option<PathBuf>,
    global_store_dir: Option<PathBuf>,
    host: Option<ExtensionHost>,
    shutdown_timeout: Duration,
}

impl ExtensionLifecycleHarness {
    pub fn new(extension: Arc<dyn Extension>) -> Result<Self, RegistrationError> {
        let registered = RegistrationHarness::register(extension.as_ref())?;
        let tasks = ExtensionTasks::new_suspended(registered.manifest().id());
        Ok(Self {
            extension,
            registered,
            tasks,
            state: LifecycleState::Registered,
            events: Vec::new(),
            config: Value::Null,
            startup_working_dir: None,
            global_store_dir: None,
            host: None,
            shutdown_timeout: Duration::from_secs(1),
        })
    }

    pub fn config(mut self, config: Value) -> Self {
        self.config = config;
        self
    }

    pub fn startup_working_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.startup_working_dir = Some(path.into());
        self
    }

    pub fn global_store_dir(mut self, path: impl Into<PathBuf>) -> Self {
        self.global_store_dir = Some(path.into());
        self
    }

    pub fn host(mut self, host: ExtensionHost) -> Self {
        self.host = Some(host);
        self
    }

    pub fn shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    pub fn registered(&self) -> &RegisteredExtension {
        &self.registered
    }

    pub fn tasks(&self) -> &ExtensionTasks {
        &self.tasks
    }

    pub fn events(&self) -> &[LifecycleHarnessEvent] {
        &self.events
    }

    pub async fn start(&mut self) -> Result<(), LifecycleHarnessError> {
        if self.state != LifecycleState::Registered {
            return Err(LifecycleHarnessError::InvalidTransition(
                "start requires a registered extension",
            ));
        }
        let extension_id = self.registered.manifest().id().to_owned();
        let mut call =
            CallContextBuilder::new(&extension_id).cancellation(self.tasks.cancellation());
        for capability in self.registered.manifest().capabilities() {
            call = call.capability(*capability);
        }
        if let Some(path) = self.startup_working_dir.clone() {
            call = call.workspace(path);
        }
        if let Some(path) = self.global_store_dir.clone() {
            call = call.global_store_dir(path);
        }
        if let Some(host) = self.host.clone() {
            call = call.host(host);
        }
        let start = ExtensionStartContext::from_runtime(
            call.build().retain_cancellation_after_context_drop(),
            self.tasks.clone(),
            ExtensionConfig::from_runtime(&extension_id, self.config.clone()),
            self.startup_working_dir.clone(),
        );
        self.events.push(LifecycleHarnessEvent::Start);
        if let Err(start_error) = self.extension.start(start).await {
            self.tasks.cancel();
            self.events.push(LifecycleHarnessEvent::TasksCancelled);
            if !self.tasks.wait(self.shutdown_timeout).await {
                return Err(LifecycleHarnessError::TasksDidNotDrain);
            }
            self.events.push(LifecycleHarnessEvent::TasksDrained);
            self.events
                .push(LifecycleHarnessEvent::Stop(StopReason::StartupFailed));
            self.state = LifecycleState::Stopped;
            return match self
                .extension
                .stop(ExtensionStopContext::from_runtime(
                    StopReason::StartupFailed,
                ))
                .await
            {
                Ok(()) => Err(LifecycleHarnessError::Extension(start_error)),
                Err(stop_error) => Err(LifecycleHarnessError::StartupRollback {
                    start: start_error.to_string(),
                    stop: stop_error.to_string(),
                }),
            };
        }
        self.tasks.activate();
        self.events.push(LifecycleHarnessEvent::TasksActivated);
        self.state = LifecycleState::Started;
        Ok(())
    }

    pub async fn stop(&mut self, reason: StopReason) -> Result<(), LifecycleHarnessError> {
        if self.state != LifecycleState::Started {
            return Err(LifecycleHarnessError::InvalidTransition(
                "stop requires a started extension",
            ));
        }
        self.tasks.cancel();
        self.events.push(LifecycleHarnessEvent::TasksCancelled);
        if !self.tasks.wait(self.shutdown_timeout).await {
            return Err(LifecycleHarnessError::TasksDidNotDrain);
        }
        self.events.push(LifecycleHarnessEvent::TasksDrained);
        self.extension
            .stop(ExtensionStopContext::from_runtime(reason))
            .await?;
        self.events.push(LifecycleHarnessEvent::Stop(reason));
        self.state = LifecycleState::Stopped;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{
        builder::manifest,
        extension::{ExtensionCall, ExtensionCapability},
        host::{HostWorkspaceReadOutput, HostWorkspaceReadRequest},
        wire::WireErrorCode,
    };

    struct LifecycleProbe;

    struct InvalidManifestProbe;

    #[async_trait]
    impl Extension for InvalidManifestProbe {
        fn manifest(&self) -> ExtensionManifest {
            manifest("invalid-manifest").build()
        }
    }

    #[async_trait]
    impl Extension for LifecycleProbe {
        fn manifest(&self) -> ExtensionManifest {
            manifest("lifecycle-probe").version("1.0.0").build()
        }

        async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
            let cancellation = ctx.cancellation().clone();
            ctx.tasks().spawn("wait-for-stop", async move {
                cancellation.cancelled().await;
            });
            Ok(())
        }
    }

    #[tokio::test]
    async fn mock_host_and_lifecycle_harness_preserve_real_policy_and_ordering() {
        assert!(matches!(
            RegistrationHarness::register(&InvalidManifestProbe),
            Err(RegistrationError::Invalid { .. })
        ));

        let ungranted = MockExtensionHost::new().workspace_context(true).respond(
            HostOperation::WorkspaceRead,
            json!(HostWorkspaceReadOutput::Text {
                content: "hello".into(),
                bytes: 5,
                total_lines: 1,
                line_offset: 0,
                returned_lines: 1,
                has_more_lines: false,
            }),
        );
        let error = ungranted
            .host()
            .workspace()
            .err()
            .expect("ungranted workspace domain must fail");
        assert_eq!(error.code_enum(), Some(WireErrorCode::PermissionDenied));

        let allowed = MockExtensionHost::new()
            .grant(ExtensionCapability::WorkspaceRead)
            .workspace_context(true)
            .respond(
                HostOperation::WorkspaceRead,
                json!(HostWorkspaceReadOutput::Text {
                    content: "hello".into(),
                    bytes: 5,
                    total_lines: 1,
                    line_offset: 0,
                    returned_lines: 1,
                    has_more_lines: false,
                }),
            );
        let output = allowed
            .host()
            .workspace()
            .expect("granted workspace domain")
            .read(HostWorkspaceReadRequest {
                path: "README.md".into(),
                max_bytes: None,
                line_offset: 0,
                line_limit: None,
            })
            .await
            .unwrap();
        assert_eq!(
            output,
            HostWorkspaceReadOutput::Text {
                content: "hello".into(),
                bytes: 5,
                total_lines: 1,
                line_offset: 0,
                returned_lines: 1,
                has_more_lines: false,
            }
        );
        assert_eq!(allowed.invocations().len(), 1);

        let mut lifecycle = ExtensionLifecycleHarness::new(Arc::new(LifecycleProbe)).unwrap();
        lifecycle.start().await.unwrap();
        lifecycle.stop(StopReason::Reload).await.unwrap();
        assert_eq!(
            lifecycle.events(),
            &[
                LifecycleHarnessEvent::Start,
                LifecycleHarnessEvent::TasksActivated,
                LifecycleHarnessEvent::TasksCancelled,
                LifecycleHarnessEvent::TasksDrained,
                LifecycleHarnessEvent::Stop(StopReason::Reload),
            ]
        );
    }
}
