//! Service dispatch over one immutable runtime view; instances remain runner-owned.
use std::{
    sync::{Arc, Weak},
    time::Duration,
};

use astrcode_extension_sdk::{
    extension::{ServiceHandler, ServiceKey, internal::service_context},
    wire::{ErrorPayload, WireErrorCode, service::ServiceInvokeRequest},
};
use serde_json::Value;

use super::{
    HandlerIndex,
    host_invoker::{ExtensionCallContextFactory, ExtensionCallContextInput},
    index::ExtensionGenerationEntry,
};
use crate::host_router::InvokeContext;

pub(super) struct ServiceEntry {
    pub handler: Arc<dyn ServiceHandler>,
    pub generation: Arc<ExtensionGenerationEntry>,
}
pub(crate) struct ServiceDispatcher {
    index: parking_lot::RwLock<Weak<HandlerIndex>>,
    factory: ExtensionCallContextFactory,
    timeout: Duration,
}
impl ServiceDispatcher {
    pub(super) fn candidate(factory: ExtensionCallContextFactory, timeout: Duration) -> Arc<Self> {
        Arc::new(Self {
            index: parking_lot::RwLock::new(Weak::new()),
            factory,
            timeout,
        })
    }
    pub(super) fn for_index(
        index: &Arc<HandlerIndex>,
        factory: ExtensionCallContextFactory,
        timeout: Duration,
    ) -> Arc<Self> {
        let dispatcher = Self::candidate(factory, timeout);
        dispatcher.bind(index);
        dispatcher
    }
    pub(super) fn bind(&self, index: &Arc<HandlerIndex>) {
        *self.index.write() = Arc::downgrade(index);
    }
    pub(crate) async fn invoke(
        &self,
        request: ServiceInvokeRequest,
        caller: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        tokio::time::timeout(self.timeout, self.invoke_inner(request, caller))
            .await
            .map_err(|_| {
                ErrorPayload::new(WireErrorCode::Timeout, "service invocation timed out")
            })?
    }
    async fn invoke_inner(
        &self,
        request: ServiceInvokeRequest,
        caller: &InvokeContext,
    ) -> Result<Value, ErrorPayload> {
        let key: ServiceKey = request
            .service
            .parse()
            .map_err(|e| ErrorPayload::new(WireErrorCode::InvalidInput, e))?;
        let index = self.index.read().upgrade().ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::HostNotReady,
                "service snapshot is not published",
            )
        })?;
        let caller_entry = index
            .extensions
            .get(&caller.extension_id)
            .filter(|entry| entry.instance_id == caller.extension_instance_id)
            .ok_or_else(|| {
                ErrorPayload::new(
                    WireErrorCode::HostNotReady,
                    "caller instance is no longer in this snapshot",
                )
            })?;
        if !caller_entry.service_permissions.contains(&key) {
            return Err(ErrorPayload::new(
                WireErrorCode::PermissionDenied,
                format!("undeclared service: {key}"),
            ));
        }
        let entry = index.services.get(&key).ok_or_else(|| {
            ErrorPayload::new(
                WireErrorCode::BackendUnavailable,
                format!("service unavailable: {key}"),
            )
        })?;
        let provider = &entry.generation;
        let mut chain = caller.service_chain.clone();
        chain.push(caller.extension_instance_id);
        if chain.len() > 8 || chain.contains(&provider.instance_id) {
            return Err(ErrorPayload::new(
                WireErrorCode::ReentrancyExceeded,
                "recursive service invocation",
            ));
        }
        let _permit = provider
            .admission
            .acquire()
            .await
            .map_err(|e| ErrorPayload::new(WireErrorCode::HostNotReady, e.to_string()))?;
        if !provider.generation_gate.is_active() {
            return Err(ErrorPayload::new(
                WireErrorCode::HostNotReady,
                "provider is not active",
            ));
        }
        let cancellation = caller
            .cancel_token
            .as_ref()
            .map(|token| token.child_token())
            .unwrap_or_default();
        let input = ExtensionCallContextInput {
            session_id: caller.session_id.clone().map(Into::into),
            tool_call_id: caller.tool_call_id.clone(),
            working_dir: caller.working_dir.as_ref().map(Into::into),
            session_store_dir: caller.session_store_dir.clone(),
            event_tx: caller.event_tx.clone(),
            event_causation: caller.event_causation.clone(),
            resource_lease: caller.resource_lease.clone(),
            file_observation_store: caller.file_observation_store.clone(),
            tool_result_reader: caller.tool_result_reader.clone(),
            llm_providers: caller.llm_providers.clone(),
            generation_gate: provider.generation_gate.clone(),
            public_http_dispatcher: caller.public_http_dispatcher.clone(),
            service_dispatcher: Some(Self::for_index(&index, self.factory.clone(), self.timeout)),
            service_chain: chain,
            cancellation,
        };
        let call = self.factory.make_extension_call_context(
            &provider.extension_id,
            provider.instance_id,
            &provider.capabilities,
            &provider.custom_event_declarations,
            provider.tasks.clone(),
            input,
        );
        let _call_lifetime = call.cancellation().clone().drop_guard();
        let context = service_context(
            call,
            caller.extension_id.clone(),
            caller.working_dir.as_ref().map(Into::into),
            caller.session_id.clone(),
        );
        let started = std::time::Instant::now();
        use astrcode_extension_sdk::extension::ExtensionCall;
        use futures_util::FutureExt;
        let cancellation = context.cancellation().clone();
        let handler = std::panic::AssertUnwindSafe(entry.handler.invoke(context, request.input))
            .catch_unwind();
        let result = tokio::select! {
            biased;
            () = cancellation.cancelled() => Err(ErrorPayload::new(WireErrorCode::Cancelled, "service invocation cancelled")),
            result = handler => match result {
                Ok(result) => result.map_err(ErrorPayload::from),
                Err(_) => Err(ErrorPayload::new(WireErrorCode::DispatchFailed, "service handler panicked")),
            },
        };
        tracing::debug!(service = %key, caller = %caller.extension_id, provider = %provider.extension_id, generation = index.generation, elapsed_ms = started.elapsed().as_millis(), success = result.is_ok(), "service invocation completed");
        result
    }
}

#[cfg(test)]
mod tests {
    use astrcode_extension_sdk::{
        builder::manifest,
        extension::*,
        host::{ExtensionHost, HostError},
    };
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;
    use crate::runner::{ExtensionRunner, SourceGenerationEntry};
    struct Probe {
        failure: tokio::sync::watch::Sender<Option<String>>,
        id: &'static str,
        forward: Option<&'static str>,
        marker: &'static str,
        starts: Arc<parking_lot::Mutex<Vec<String>>>,
        hosts: Arc<parking_lot::Mutex<Vec<ExtensionHost>>>,
    }
    #[async_trait::async_trait]
    impl Extension for Probe {
        fn runtime_failure(&self) -> Option<tokio::sync::watch::Receiver<Option<String>>> {
            Some(self.failure.subscribe())
        }
        fn manifest(&self) -> ExtensionManifest {
            let mut builder = manifest(self.id).version("1");
            if let Some(forward) = self.forward {
                builder = builder.dependency(forward.parse().unwrap(), DependencyKind::Required);
            }
            builder
                .allow_service(format!("{}@1", self.id).parse().unwrap())
                .build()
        }
        fn register(&self, registrar: &mut Registrar) {
            registrar.service(
                format!("{}@1", self.id),
                Arc::new(Echo {
                    retained_hosts: self.hosts.clone(),
                    failure: self.failure.clone(),
                    marker: self.marker,
                    forward: self.forward,
                }),
            );
        }
        async fn stop(&self, _: ExtensionStopContext) -> Result<(), ExtensionError> {
            self.starts
                .lock()
                .push(format!("stop:{}:{}", self.marker, self.id));
            Ok(())
        }
        async fn start(&self, context: ExtensionStartContext) -> Result<(), ExtensionError> {
            let key = format!("{}@1", self.id).parse().unwrap();
            let error = context
                .host()
                .services()?
                .invoke(&key, json!({}))
                .await
                .unwrap_err();
            assert_eq!(error.code, WireErrorCode::HostNotReady.as_str());
            self.starts.lock().push(self.id.into());
            self.hosts.lock().push(context.host().clone());
            Ok(())
        }
    }
    struct Echo {
        retained_hosts: Arc<parking_lot::Mutex<Vec<ExtensionHost>>>,
        failure: tokio::sync::watch::Sender<Option<String>>,
        marker: &'static str,
        forward: Option<&'static str>,
    }
    #[async_trait::async_trait]
    impl ServiceHandler for Echo {
        async fn invoke(&self, context: ServiceContext, input: Value) -> Result<Value, HostError> {
            if input == json!("retain") {
                self.retained_hosts.lock().push(context.host().clone());
                return Ok(Value::Null);
            }
            if input == json!("fail") {
                self.failure.send_replace(Some("worker stopped".into()));
                return Err(HostError::new(
                    WireErrorCode::BackendUnavailable,
                    "worker stopped",
                ));
            }
            if input == json!("error") {
                return Err(HostError {
                    code: "custom_failure".into(),
                    message: "failed".into(),
                    hint: Some("hint".into()),
                    retryable: true,
                    details: Some(json!({"marker": self.marker})),
                });
            }
            if input == json!("recursive") {
                return context
                    .host()
                    .services()?
                    .invoke(
                        &format!("{}@1", context.extension_id()).parse().unwrap(),
                        input,
                    )
                    .await;
            }
            if let Some(forward) = self.forward {
                return context
                    .host()
                    .services()?
                    .invoke(&forward.parse().unwrap(), input)
                    .await;
            }
            Ok(
                json!({"marker": self.marker, "caller": context.caller_extension_id(), "workspace": context.working_dir(), "session": context.session_id()}),
            )
        }
    }
    async fn prepare(
        runner: &Arc<ExtensionRunner>,
        marker: &'static str,
        provider: bool,
        starts: &Arc<parking_lot::Mutex<Vec<String>>>,
        hosts: &Arc<parking_lot::Mutex<Vec<ExtensionHost>>>,
    ) -> super::super::PreparedExtensionGeneration {
        let mut items = vec![("client", Some("middle@1")), ("middle", Some("base@1"))];
        if provider {
            items.push(("base", None));
        }
        let entries = items
            .into_iter()
            .map(|(id, forward)| SourceGenerationEntry::Start {
                extension: Arc::new(Probe {
                    failure: tokio::sync::watch::channel(None).0,
                    id,
                    forward,
                    marker,
                    starts: starts.clone(),
                    hosts: hosts.clone(),
                }),
                key: id.into(),
                fingerprint: marker.into(),
                config: json!({}),
            })
            .collect();
        runner
            .prepare_source_generation(
                runner.begin_source_transaction().await,
                entries,
                Some("/tmp"),
            )
            .await
            .unwrap()
    }
    #[tokio::test]
    async fn service_calls_preserve_permissions_context_snapshots_and_dependency_recovery() {
        let runner = Arc::new(ExtensionRunner::new(Duration::from_secs(2)));
        let starts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let hosts = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let candidate = prepare(&runner, "old", true, &starts, &hosts).await;
        assert_eq!(*starts.lock(), ["base", "middle", "client"]);
        let startup_host = hosts.lock()[0].clone();
        assert_eq!(
            startup_host
                .services()
                .unwrap()
                .invoke(&"base@1".parse().unwrap(), json!({}))
                .await
                .unwrap_err()
                .code,
            WireErrorCode::HostNotReady.as_str()
        );
        candidate.commit_with(|_| {}).await;
        let old = runner.extension_view().await;
        let call = old
            .make_registered_extension_call_context(
                "client",
                ExtensionCallContextInput {
                    working_dir: Some("/trusted".into()),
                    session_id: Some("session".into()),
                    ..ExtensionCallContextInput::unscoped(CancellationToken::new())
                },
            )
            .unwrap();
        let services = call.host().services().unwrap();
        let output = services
            .invoke(&"middle@1".parse().unwrap(), json!({"caller":"forged"}))
            .await
            .unwrap();
        assert_eq!(
            output,
            json!({"marker":"old", "caller":"middle", "workspace":"/trusted", "session":"session"})
        );
        assert_eq!(
            services
                .invoke(&"base@1".parse().unwrap(), json!({}))
                .await
                .unwrap_err()
                .code,
            WireErrorCode::PermissionDenied.as_str()
        );
        assert_eq!(
            services
                .invoke(&"client@1".parse().unwrap(), json!("recursive"))
                .await
                .unwrap_err()
                .code,
            WireErrorCode::ReentrancyExceeded.as_str()
        );
        services
            .invoke(&"middle@1".parse().unwrap(), json!("retain"))
            .await
            .unwrap();
        let retained_host = hosts.lock().last().unwrap().clone();
        assert_eq!(
            retained_host
                .services()
                .unwrap()
                .invoke(&"base@1".parse().unwrap(), json!({}))
                .await
                .unwrap_err()
                .code,
            WireErrorCode::Cancelled.as_str()
        );
        let error = services
            .invoke(&"middle@1".parse().unwrap(), json!("error"))
            .await
            .unwrap_err();
        assert_eq!(error.code, "custom_failure");
        assert!(error.retryable);
        assert_eq!(error.hint.as_deref(), Some("hint"));
        prepare(&runner, "new", true, &starts, &hosts)
            .await
            .commit_with(|_| {})
            .await;
        assert_eq!(
            services
                .invoke(&"middle@1".parse().unwrap(), json!({}))
                .await
                .unwrap()["marker"],
            "old"
        );
        assert!(
            services
                .invoke(&"middle@1".parse().unwrap(), json!("fail"))
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let Some(result) = runner.failure_observers.lock().try_join_next() {
                    result.unwrap();
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(
            runner
                .registry_snapshot()
                .await
                .extensions
                .iter()
                .all(|d| d.blocked_reasons.is_empty())
        );
        drop(services);
        drop(call);
        drop(old);
        hosts.lock().clear();
        prepare(&runner, "disabled", false, &starts, &hosts)
            .await
            .commit_with(|_| {})
            .await;
        let snapshot = runner.registry_snapshot().await;
        assert_eq!(snapshot.extensions.len(), 2);
        assert!(
            snapshot
                .extensions
                .iter()
                .all(|d| !d.blocked_reasons.is_empty())
        );
        prepare(&runner, "restored", true, &starts, &hosts)
            .await
            .commit_with(|_| {})
            .await;
        assert!(
            runner
                .registry_snapshot()
                .await
                .extensions
                .iter()
                .all(|d| d.blocked_reasons.is_empty())
        );
        hosts.lock().clear();
        let published = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let observed = published.clone();
        runner.bind_runtime_change_publisher(move |generation| {
            observed.store(generation, std::sync::atomic::Ordering::SeqCst)
        });
        let view = runner.extension_view().await;
        let call = view
            .make_registered_extension_call_context(
                "client",
                ExtensionCallContextInput::unscoped(CancellationToken::new()),
            )
            .unwrap();
        assert!(
            call.host()
                .services()
                .unwrap()
                .invoke(&"middle@1".parse().unwrap(), json!("fail"))
                .await
                .is_err()
        );
        drop(call);
        drop(view);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = runner.registry_snapshot().await;
                if snapshot.extensions.iter().any(|d| {
                    d.id == "middle"
                        && d.runtime_state == super::super::ExtensionRuntimeState::Failed
                }) {
                    assert!(
                        snapshot
                            .extensions
                            .iter()
                            .any(|d| d.id == "client" && !d.blocked_reasons.is_empty())
                    );
                    assert_eq!(
                        published.load(std::sync::atomic::Ordering::SeqCst),
                        snapshot.extensions[0].generation
                    );
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        prepare(&runner, "recovered", true, &starts, &hosts)
            .await
            .commit_with(|_| {})
            .await;
        assert!(
            runner
                .registry_snapshot()
                .await
                .extensions
                .iter()
                .all(|d| d.blocked_reasons.is_empty())
        );
        hosts.lock().clear();
        assert!(runner.shutdown().await.is_empty());
        let records = starts.lock();
        for marker in ["old", "new", "restored"] {
            let position = |id| {
                records
                    .iter()
                    .position(|record| record == &format!("stop:{marker}:{id}"))
                    .unwrap()
            };
            assert!(position("client") < position("middle"), "{records:?}");
            assert!(position("middle") < position("base"), "{records:?}");
        }
    }
}
