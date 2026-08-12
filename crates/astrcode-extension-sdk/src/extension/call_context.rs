//! Host-attributed context shared by extension lifecycle and handler calls.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use astrcode_core::types::SessionId;
use tokio_util::sync::CancellationToken;

use super::{CustomEventEmitter, ExtensionConfig, ExtensionPaths, ExtensionTasks};
use crate::host::ExtensionHost;

/// Facts and scoped capabilities shared by extension calls.
///
/// The runtime owns construction so authors cannot replace extension attribution, capability
/// grants, persistence namespaces, or cancellation ownership with request-controlled values.
#[derive(Clone)]
pub struct ExtensionCallContext {
    extension_id: Arc<str>,
    paths: ExtensionPaths,
    host: ExtensionHost,
    events: CustomEventEmitter,
    tasks: ExtensionTasks,
    cancellation: Arc<CallCancellation>,
}

struct CallCancellation {
    token: CancellationToken,
    cancel_on_drop: AtomicBool,
}

impl Drop for CallCancellation {
    fn drop(&mut self) {
        if self.cancel_on_drop.load(Ordering::Acquire) {
            self.token.cancel();
        }
    }
}

impl ExtensionCallContext {
    #[doc(hidden)]
    #[allow(clippy::too_many_arguments)]
    pub fn from_runtime(
        extension_id: impl Into<String>,
        paths: ExtensionPaths,
        host: ExtensionHost,
        events: CustomEventEmitter,
        tasks: ExtensionTasks,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            extension_id: Arc::from(extension_id.into()),
            paths,
            host,
            events,
            tasks,
            cancellation: Arc::new(CallCancellation {
                token: cancellation,
                cancel_on_drop: AtomicBool::new(true),
            }),
        }
    }

    /// Keeps the cancellation signal alive after this context is dropped.
    ///
    /// The runtime uses this only for startup contexts whose signal represents the whole
    /// extension generation and may be cloned into managed background tasks.
    #[doc(hidden)]
    pub fn retain_cancellation_after_context_drop(self) -> Self {
        self.cancellation
            .cancel_on_drop
            .store(false, Ordering::Release);
        self
    }

    pub fn extension_id(&self) -> &str {
        &self.extension_id
    }

    pub fn paths(&self) -> &ExtensionPaths {
        &self.paths
    }

    pub fn host(&self) -> &ExtensionHost {
        &self.host
    }

    pub fn events(&self) -> &CustomEventEmitter {
        &self.events
    }

    pub fn tasks(&self) -> &ExtensionTasks {
        &self.tasks
    }

    pub fn cancellation(&self) -> &CancellationToken {
        &self.cancellation.token
    }
}

/// Read-only access to the host-attributed call shared by every extension context.
///
/// Each context type implements only [`ExtensionCall::call`]; all delegating accessors are
/// provided methods, so names, return types, and error codes/messages cannot drift between
/// contexts.
pub trait ExtensionCall {
    fn call(&self) -> &ExtensionCallContext;

    fn extension_id(&self) -> &str {
        self.call().extension_id()
    }

    fn paths(&self) -> &ExtensionPaths {
        self.call().paths()
    }

    fn host(&self) -> &ExtensionHost {
        self.call().host()
    }

    fn events(&self) -> &CustomEventEmitter {
        self.call().events()
    }

    fn tasks(&self) -> &ExtensionTasks {
        self.call().tasks()
    }

    fn cancellation(&self) -> &CancellationToken {
        self.call().cancellation()
    }
}

/// Host-attributed facts guaranteed for a workspace-scoped invocation.
#[derive(Clone)]
pub struct WorkspaceCallContext {
    call: ExtensionCallContext,
    working_dir: PathBuf,
}

impl WorkspaceCallContext {
    #[doc(hidden)]
    pub fn from_runtime(call: ExtensionCallContext, working_dir: impl Into<PathBuf>) -> Self {
        Self {
            call,
            working_dir: working_dir.into(),
        }
    }

    pub fn working_dir(&self) -> &Path {
        &self.working_dir
    }
}

impl ExtensionCall for WorkspaceCallContext {
    fn call(&self) -> &ExtensionCallContext {
        &self.call
    }
}

impl std::fmt::Debug for WorkspaceCallContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkspaceCallContext")
            .field("call", &self.call)
            .field("working_dir", &self.working_dir)
            .finish()
    }
}

/// Host-attributed facts guaranteed for a session-scoped invocation.
#[derive(Clone)]
pub struct SessionCallContext {
    call: ExtensionCallContext,
    session_id: SessionId,
    turn_id: Option<Arc<str>>,
}

impl SessionCallContext {
    #[doc(hidden)]
    pub fn from_runtime(
        call: ExtensionCallContext,
        session_id: SessionId,
        turn_id: Option<String>,
    ) -> Self {
        Self {
            call,
            session_id,
            turn_id: turn_id.map(Arc::from),
        }
    }

    pub fn session_id(&self) -> &SessionId {
        &self.session_id
    }

    pub fn turn_id(&self) -> Option<&str> {
        self.turn_id.as_deref()
    }
}

impl ExtensionCall for SessionCallContext {
    fn call(&self) -> &ExtensionCallContext {
        &self.call
    }
}

impl std::fmt::Debug for SessionCallContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionCallContext")
            .field("call", &self.call)
            .field("session_id", &self.session_id)
            .field("turn_id", &self.turn_id)
            .finish()
    }
}

impl std::fmt::Debug for ExtensionCallContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionCallContext")
            .field("extension_id", &self.extension_id)
            .field("paths", &self.paths)
            .field("events", &self.events)
            .field("cancelled", &self.cancellation.token.is_cancelled())
            .finish_non_exhaustive()
    }
}

/// Lifecycle context passed to [`super::Extension::start`].
///
/// Startup is extension-scoped rather than session-scoped: session and turn attribution are
/// absent, and session-only host operations return a context-unavailable error. The optional
/// working directory is only the workspace known when the extension was loaded.
#[derive(Clone)]
pub struct ExtensionStartContext {
    call: ExtensionCallContext,
    config: ExtensionConfig,
    startup_working_dir: Option<PathBuf>,
}

impl ExtensionStartContext {
    #[doc(hidden)]
    pub fn from_runtime(
        call: ExtensionCallContext,
        config: ExtensionConfig,
        startup_working_dir: Option<PathBuf>,
    ) -> Self {
        Self {
            call,
            config,
            startup_working_dir,
        }
    }

    pub fn config(&self) -> &ExtensionConfig {
        &self.config
    }

    pub fn startup_working_dir(&self) -> Option<&Path> {
        self.startup_working_dir.as_deref()
    }
}

impl ExtensionCall for ExtensionStartContext {
    fn call(&self) -> &ExtensionCallContext {
        &self.call
    }
}

impl std::fmt::Debug for ExtensionStartContext {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ExtensionStartContext")
            .field("call", &self.call)
            .field("config", &"<redacted>")
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::{
        extension::ExtensionCapability,
        host::{
            HostError, HostOperation,
            internal::{HostInvoker, HostScope, extension_host},
        },
    };

    struct UnusedHost;

    #[async_trait]
    impl HostInvoker for UnusedHost {
        async fn invoke(
            &self,
            operation: HostOperation,
            _input: Value,
        ) -> Result<Value, HostError> {
            Err(HostError::new(
                crate::WireErrorCode::InternalError,
                format!("unexpected operation: {}", operation.wire_name()),
            ))
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
    }

    #[test]
    fn startup_context_exposes_runtime_attribution_without_debugging_config_secrets() {
        let host = extension_host(
            Arc::new(UnusedHost),
            HostScope::new(
                std::iter::empty::<ExtensionCapability>(),
                std::iter::empty::<HostOperation>(),
                false,
                true,
            ),
        );
        let cancellation = CancellationToken::new();
        let call = ExtensionCallContext::from_runtime(
            "startup-probe",
            ExtensionPaths::from_runtime("startup-probe", Some(Path::new("/global")), None),
            host,
            CustomEventEmitter::default(),
            ExtensionTasks::new("startup-probe"),
            cancellation,
        );
        let ctx = ExtensionStartContext::from_runtime(
            call,
            ExtensionConfig::from_runtime("startup-probe", json!({ "token": "secret" })),
            Some(PathBuf::from("/workspace")),
        );

        assert_eq!(ctx.extension_id(), "startup-probe");
        assert_eq!(ctx.startup_working_dir(), Some(Path::new("/workspace")));
        assert_eq!(
            ctx.paths().global_data_dir(),
            Some(Path::new("/global/extension_data/startup-probe"))
        );
        assert!(ctx.paths().session_data_dir().is_err());
        assert_eq!(
            ctx.config().deserialize::<Value>().unwrap(),
            json!({ "token": "secret" })
        );
        assert!(!ctx.cancellation().is_cancelled());

        let debug = format!("{ctx:?}");
        assert!(debug.contains("<redacted>"));
        assert!(!debug.contains("secret"));
    }

    #[test]
    fn dropping_the_last_call_context_cancels_its_call_lifetime() {
        let host = extension_host(
            Arc::new(UnusedHost),
            HostScope::new(
                std::iter::empty::<ExtensionCapability>(),
                std::iter::empty::<HostOperation>(),
                false,
                false,
            ),
        );
        let cancellation = CancellationToken::new();
        let observer = cancellation.clone();
        let call = ExtensionCallContext::from_runtime(
            "drop-probe",
            ExtensionPaths::default(),
            host,
            CustomEventEmitter::default(),
            ExtensionTasks::new("drop-probe"),
            cancellation,
        );

        let clone = call.clone();
        drop(call);
        assert!(!observer.is_cancelled());
        drop(clone);
        assert!(observer.is_cancelled());
    }

    #[test]
    fn generation_lifetime_outlives_the_startup_context() {
        let host = extension_host(
            Arc::new(UnusedHost),
            HostScope::new(
                std::iter::empty::<ExtensionCapability>(),
                std::iter::empty::<HostOperation>(),
                false,
                false,
            ),
        );
        let cancellation = CancellationToken::new();
        let observer = cancellation.clone();
        let call = ExtensionCallContext::from_runtime(
            "generation-probe",
            ExtensionPaths::default(),
            host,
            CustomEventEmitter::default(),
            ExtensionTasks::new("generation-probe"),
            cancellation,
        )
        .retain_cancellation_after_context_drop();

        drop(call);
        assert!(!observer.is_cancelled());
    }
}
