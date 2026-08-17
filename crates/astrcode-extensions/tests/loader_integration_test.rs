//! 集成测试：扩展加载器边界条件与 manifest 解析。

use std::{
    collections::BTreeMap,
    sync::{
        Arc, Weak,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

use astrcode_extension_sdk::{
    builder::{
        command, command_handler, http_handler, http_route, keybinding, manifest, status_item,
        tool, tool_handler,
    },
    extension::{
        Extension, ExtensionCapability, ExtensionCommandResult, ExtensionError,
        ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionManifest,
        ExtensionStartContext, Registrar,
    },
    runtime_ports::{RuntimeSnapshotProvider, RuntimeSnapshotState},
    tool::{ToolPlan, ToolResult},
    transport::{TransportFeature, TransportProfile},
};
use astrcode_extensions::{
    loader::{
        DiscoverExtensionsResult, ExtensionCandidate, ExtensionLoadContext, ExtensionLoadFailure,
        ExtensionSource, prepare_extension_generation,
    },
    runner::{ExtensionHttpDispatchResult, ExtensionRunner, ExtensionStageStatus},
};
use tokio::sync::Notify;

async fn sync_extension_sources(
    runner: &Arc<ExtensionRunner>,
    ctx: &ExtensionLoadContext,
    sources: &[&dyn ExtensionSource],
) -> Vec<String> {
    match prepare_extension_generation(runner, ctx, sources, &BTreeMap::new()).await {
        Ok(candidate) => {
            candidate.commit_with(|_| {}).await;
            Vec::new()
        },
        Err(errors) => errors,
    }
}

fn test_manifest(id: impl Into<String>) -> ExtensionManifest {
    manifest(id)
        .version("test")
        .description("Extension loader test probe")
        .build()
}

struct BrokenSource;

struct BatchSource {
    extensions: Vec<Arc<dyn Extension>>,
}

struct BatchObserverExtension {
    runner: Weak<ExtensionRunner>,
    start_returned: Arc<AtomicBool>,
    ran_during_start: Arc<AtomicBool>,
    saw_complete_batch: Arc<AtomicBool>,
    completed: Arc<Notify>,
}

struct NamedExtension(&'static str);

struct AuthenticatedHttpExtension;

struct CatalogExtension {
    id: &'static str,
    tool_name: &'static str,
    start_barrier: Option<(Arc<Notify>, Arc<Notify>)>,
}

#[async_trait::async_trait]
impl ExtensionSource for BrokenSource {
    async fn discover(&self, _ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult {
        DiscoverExtensionsResult {
            candidates: Vec::new(),
            failures: vec![ExtensionLoadFailure {
                source_key: None,
                extension_id: Some("broken-extension".into()),
                message: "broken extension failed".into(),
                duration_ms: None,
            }],
        }
    }
}

#[async_trait::async_trait]
impl ExtensionSource for BatchSource {
    async fn discover(&self, _ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult {
        let candidates = self
            .extensions
            .iter()
            .map(|extension| {
                let id = extension.manifest().id().to_owned();
                ExtensionCandidate::ready(
                    format!("test:{id}"),
                    format!("test-v1:{id}"),
                    Arc::clone(extension),
                )
            })
            .collect();
        DiscoverExtensionsResult {
            candidates,
            ..Default::default()
        }
    }
}

#[async_trait::async_trait]
impl Extension for BatchObserverExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_manifest("batch-observer")
    }

    async fn start(&self, ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        let runner = self.runner.clone();
        let start_returned = Arc::clone(&self.start_returned);
        let ran_during_start = Arc::clone(&self.ran_during_start);
        let saw_complete_batch = Arc::clone(&self.saw_complete_batch);
        let completed = Arc::clone(&self.completed);
        ctx.tasks().spawn("observe-registration-batch", async move {
            ran_during_start.store(!start_returned.load(Ordering::SeqCst), Ordering::SeqCst);
            if let Some(runner) = runner.upgrade() {
                saw_complete_batch.store(
                    runner
                        .registered_extension_ids()
                        .await
                        .iter()
                        .any(|id| id == "batch-second"),
                    Ordering::SeqCst,
                );
            }
            completed.notify_one();
        });
        tokio::task::yield_now().await;
        self.start_returned.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait::async_trait]
impl Extension for NamedExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_manifest(self.0)
    }
}

#[async_trait::async_trait]
impl Extension for AuthenticatedHttpExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest("astrcode-ask-user")
            .version("test")
            .description("Authenticated HTTP admission probe")
            .requires_transport(TransportFeature::AuthenticatedHttp)
            .build()
    }
}

#[async_trait::async_trait]
impl Extension for CatalogExtension {
    fn manifest(&self) -> ExtensionManifest {
        manifest(self.id)
            .version("test")
            .description("Atomic reload catalog probe")
            .capability(ExtensionCapability::PublicHttp)
            .build()
    }

    fn register(&self, reg: &mut Registrar) {
        reg.tool(
            tool(self.tool_name)
                .description("Atomic reload catalog probe")
                .build(),
            tool_handler(
                |_| async { Ok(ToolPlan::default()) },
                |_| async { Ok(ToolResult::text("ok".into(), false, Default::default())) },
            ),
        );
        reg.command(
            command(self.tool_name)
                .description("Atomic reload command probe")
                .build(),
            command_handler(|_| async { Ok(ExtensionCommandResult::handled("ok")) }),
        );
        reg.keybinding(keybinding(format!("test+{}", self.id), self.tool_name).build());
        reg.status_item(status_item(self.id, self.tool_name).build());
        let tool_name = self.tool_name;
        reg.http_route(
            http_route(ExtensionHttpMethod::Get, format!("/catalog/{}", self.id))
                .public()
                .build(),
            http_handler(move |_| async move {
                Ok(ExtensionHttpResponse::json(
                    200,
                    serde_json::json!({ "tool": tool_name }),
                ))
            }),
        );
    }

    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        if let Some((entered, release)) = &self.start_barrier {
            entered.notify_one();
            release.notified().await;
        }
        Ok(())
    }
}

async fn published_tool_catalog(runner: &ExtensionRunner) -> (u64, Vec<String>) {
    let catalog = runner.tool_catalog_snapshot_typed("/workspace").await;
    let mut names = catalog
        .tools
        .iter()
        .map(|tool| tool.definition().name)
        .collect::<Vec<_>>();
    names.sort();
    (catalog.revision, names)
}

async fn published_http_tool(runner: &ExtensionRunner, path: &str) -> String {
    match runner
        .dispatch_public_http_route(
            ExtensionHttpRequest::new(ExtensionHttpMethod::Get, path),
            &[],
        )
        .await
        .unwrap()
    {
        ExtensionHttpDispatchResult::Response(response) => response.body["tool"]
            .as_str()
            .expect("HTTP probe should return a tool name")
            .to_owned(),
        _ => panic!("HTTP probe route should be published"),
    }
}

struct CountingExtension {
    id: &'static str,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Extension for CountingExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_manifest(self.id)
    }

    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(
        &self,
        _ctx: astrcode_extension_sdk::extension::ExtensionStopContext,
    ) -> Result<(), ExtensionError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct StartFailingExtension {
    id: &'static str,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Extension for StartFailingExtension {
    fn manifest(&self) -> ExtensionManifest {
        test_manifest(self.id)
    }

    async fn start(&self, _ctx: ExtensionStartContext) -> Result<(), ExtensionError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Err(ExtensionError::Internal(
            "injected candidate startup failure".into(),
        ))
    }

    async fn stop(
        &self,
        _ctx: astrcode_extension_sdk::extension::ExtensionStopContext,
    ) -> Result<(), ExtensionError> {
        self.stops.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct FingerprintSource {
    entries: Vec<(&'static str, &'static str, Arc<dyn Extension>)>,
    loads: Arc<AtomicUsize>,
}

struct UnavailableFingerprintSource;

struct DisabledCandidateSource {
    loads: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl ExtensionSource for FingerprintSource {
    async fn discover(&self, _ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult {
        DiscoverExtensionsResult {
            candidates: self
                .entries
                .iter()
                .map(|(source_key, fingerprint, extension)| {
                    let extension_id = extension.manifest().id().to_owned();
                    let extension = Arc::clone(extension);
                    let loads = Arc::clone(&self.loads);
                    ExtensionCandidate::lazy(
                        *source_key,
                        *fingerprint,
                        extension_id,
                        move || async move {
                            loads.fetch_add(1, Ordering::SeqCst);
                            Ok(extension)
                        },
                    )
                })
                .collect(),
            ..Default::default()
        }
    }
}

#[async_trait::async_trait]
impl ExtensionSource for UnavailableFingerprintSource {
    async fn discover(&self, _ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult {
        DiscoverExtensionsResult {
            failures: vec![ExtensionLoadFailure {
                source_key: None,
                extension_id: None,
                message: "source unavailable".into(),
                duration_ms: None,
            }],
            ..Default::default()
        }
    }
}

#[async_trait::async_trait]
impl ExtensionSource for DisabledCandidateSource {
    async fn discover(&self, _ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult {
        let loads = Arc::clone(&self.loads);
        DiscoverExtensionsResult {
            candidates: vec![ExtensionCandidate::lazy(
                "source:disabled",
                "v1",
                "disabled-extension",
                move || async move {
                    loads.fetch_add(1, Ordering::SeqCst);
                    Ok(Arc::new(NamedExtension("disabled-extension")) as Arc<dyn Extension>)
                },
            )],
            ..Default::default()
        }
    }

    fn is_enabled(&self, extension_id: &str) -> bool {
        assert_eq!(extension_id, "disabled-extension");
        false
    }
}

#[tokio::test]
async fn sync_sources_records_load_failure_diagnostics() {
    let runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
    let source = BrokenSource;
    let errors = sync_extension_sources(
        &runner,
        &ExtensionLoadContext {
            working_dir: None,
            host_router: None,
            transport_profile: Default::default(),
        },
        &[&source],
    )
    .await;

    assert_eq!(errors, vec!["broken extension failed"]);
    let diagnostics = runner.diagnostics_snapshot();
    let diagnostics = diagnostics.get("broken-extension").unwrap();
    assert_eq!(diagnostics.load.status, ExtensionStageStatus::Failed);
    assert_eq!(
        diagnostics.load.error.as_deref(),
        Some("broken extension failed")
    );
}

#[tokio::test]
async fn sync_sources_does_not_load_disabled_candidates() {
    let runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
    let loads = Arc::new(AtomicUsize::new(0));
    let source = DisabledCandidateSource {
        loads: Arc::clone(&loads),
    };

    let errors = sync_extension_sources(
        &runner,
        &ExtensionLoadContext {
            working_dir: None,
            host_router: None,
            transport_profile: Default::default(),
        },
        &[&source],
    )
    .await;

    assert!(errors.is_empty());
    assert_eq!(loads.load(Ordering::SeqCst), 0);
    assert!(runner.registered_extension_ids().await.is_empty());
}

#[tokio::test]
async fn transport_profiles_admit_only_extensions_with_satisfied_requirements() {
    for (transport, profile, should_load) in [
        ("stdio", TransportProfile::default(), false),
        ("acp", TransportProfile::default(), false),
        (
            "http",
            TransportProfile::new([TransportFeature::AuthenticatedHttp]),
            true,
        ),
    ] {
        let runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
        let source = BatchSource {
            extensions: vec![Arc::new(AuthenticatedHttpExtension)],
        };

        let errors = sync_extension_sources(
            &runner,
            &ExtensionLoadContext {
                working_dir: None,
                host_router: None,
                transport_profile: profile,
            },
            &[&source],
        )
        .await;

        assert!(errors.is_empty(), "{transport} load failed: {errors:?}");
        assert_eq!(
            runner.registered_extension_ids().await == ["astrcode-ask-user"],
            should_load,
            "unexpected {transport} admission result"
        );
        let diagnostics = runner.diagnostics_snapshot();
        let load = &diagnostics["astrcode-ask-user"].load;
        if should_load {
            assert_eq!(load.status, ExtensionStageStatus::Succeeded);
        } else {
            assert_eq!(load.status, ExtensionStageStatus::Failed);
            assert!(
                load.error
                    .as_deref()
                    .is_some_and(|error| error.contains("authenticated_http")),
                "{transport} rejection must name the missing transport feature"
            );
        }
        assert!(runner.shutdown().await.is_empty());
    }
}

#[tokio::test]
async fn sync_sources_activates_tasks_after_publishing_the_complete_batch() {
    let runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
    let start_returned = Arc::new(AtomicBool::new(false));
    let ran_during_start = Arc::new(AtomicBool::new(false));
    let saw_complete_batch = Arc::new(AtomicBool::new(false));
    let completed = Arc::new(Notify::new());
    let source = BatchSource {
        extensions: vec![
            Arc::new(BatchObserverExtension {
                runner: Arc::downgrade(&runner),
                start_returned: Arc::clone(&start_returned),
                ran_during_start: Arc::clone(&ran_during_start),
                saw_complete_batch: Arc::clone(&saw_complete_batch),
                completed: Arc::clone(&completed),
            }),
            Arc::new(NamedExtension("batch-second")),
        ],
    };

    let errors = sync_extension_sources(
        &runner,
        &ExtensionLoadContext {
            working_dir: None,
            host_router: None,
            transport_profile: Default::default(),
        },
        &[&source],
    )
    .await;
    tokio::time::timeout(std::time::Duration::from_secs(1), completed.notified())
        .await
        .unwrap();

    assert!(errors.is_empty());
    assert!(!ran_during_start.load(Ordering::SeqCst));
    assert!(saw_complete_batch.load(Ordering::SeqCst));
}

#[tokio::test]
async fn sync_sources_publishes_reload_batches_as_one_coherent_generation() {
    let runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
    let ctx = ExtensionLoadContext {
        working_dir: None,
        host_router: None,
        transport_profile: Default::default(),
    };
    let initial = FingerprintSource {
        entries: vec![
            (
                "source:catalog-a",
                "v1",
                Arc::new(CatalogExtension {
                    id: "catalog-a",
                    tool_name: "oldA",
                    start_barrier: None,
                }),
            ),
            (
                "source:catalog-b",
                "v1",
                Arc::new(CatalogExtension {
                    id: "catalog-b",
                    tool_name: "oldB",
                    start_barrier: None,
                }),
            ),
        ],
        loads: Arc::new(AtomicUsize::new(0)),
    };
    assert!(
        sync_extension_sources(&runner, &ctx, &[&initial])
            .await
            .is_empty()
    );
    let (published_initial_generation, initial_names) = published_tool_catalog(&runner).await;
    assert_eq!(initial_names, ["oldA", "oldB"]);
    let initial_generation = match runner.runtime_snapshot_state() {
        RuntimeSnapshotState::Stable(generation) => generation,
        RuntimeSnapshotState::Updating => unreachable!("completed sync must be stable"),
    };
    assert_eq!(published_initial_generation, initial_generation);
    assert!(
        sync_extension_sources(&runner, &ctx, &[&initial])
            .await
            .is_empty()
    );
    assert_eq!(
        runner.runtime_snapshot_state(),
        RuntimeSnapshotState::Stable(initial_generation),
        "an unchanged source batch must not publish a new generation"
    );

    let start_entered = Arc::new(Notify::new());
    let start_release = Arc::new(Notify::new());
    let reload = {
        let runner = Arc::clone(&runner);
        let start_entered = Arc::clone(&start_entered);
        let start_release = Arc::clone(&start_release);
        tokio::spawn(async move {
            let updated = FingerprintSource {
                entries: vec![
                    (
                        "source:catalog-a",
                        "v2",
                        Arc::new(CatalogExtension {
                            id: "catalog-a",
                            tool_name: "newA",
                            start_barrier: None,
                        }),
                    ),
                    (
                        "source:catalog-b",
                        "v2",
                        Arc::new(CatalogExtension {
                            id: "catalog-b",
                            tool_name: "newB",
                            start_barrier: Some((start_entered, start_release)),
                        }),
                    ),
                ],
                loads: Arc::new(AtomicUsize::new(0)),
            };
            sync_extension_sources(
                &runner,
                &ExtensionLoadContext {
                    working_dir: None,
                    host_router: None,
                    transport_profile: Default::default(),
                },
                &[&updated],
            )
            .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), start_entered.notified())
        .await
        .expect("second replacement should reach its start barrier");
    assert_eq!(
        runner.runtime_snapshot_state(),
        RuntimeSnapshotState::Stable(initial_generation),
        "candidate startup must not disturb the committed generation"
    );

    let concurrent_reader = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { published_tool_catalog(&runner).await })
    };
    let concurrent_http_reader = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { published_http_tool(&runner, "/catalog/catalog-b").await })
    };
    let concurrent_registry_reader = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.registry_snapshot().await })
    };
    let concurrent_command_surface = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.resolve_command_surface("/workspace").await })
    };
    let (concurrent_generation, concurrent_tools) =
        tokio::time::timeout(std::time::Duration::from_secs(1), concurrent_reader)
            .await
            .expect("readers should keep using the committed generation")
            .unwrap();
    assert_eq!(concurrent_generation, initial_generation);
    assert_eq!(concurrent_tools, ["oldA", "oldB"]);
    assert_eq!(
        concurrent_http_reader.await.unwrap(),
        "oldB",
        "candidate routes must remain unpublished"
    );
    let concurrent_registry = concurrent_registry_reader.await.unwrap();
    assert!(
        concurrent_registry
            .extensions
            .iter()
            .flat_map(|extension| &extension.tools)
            .all(|tool| tool.name.starts_with("old"))
    );
    assert!(
        concurrent_command_surface
            .await
            .unwrap()
            .commands
            .iter()
            .all(|command| command.command.name.starts_with("old"))
    );

    start_release.notify_one();
    assert!(reload.await.unwrap().is_empty());
    let (final_generation, final_names) = published_tool_catalog(&runner).await;
    assert_eq!(final_names, ["newA", "newB"]);
    assert!(final_generation > initial_generation);
    assert_eq!(
        runner.runtime_snapshot_state(),
        RuntimeSnapshotState::Stable(final_generation)
    );
    assert_eq!(
        published_http_tool(&runner, "/catalog/catalog-b").await,
        "newB"
    );
    let registry = runner.registry_snapshot().await;
    let mut registry_tools = registry
        .extensions
        .into_iter()
        .flat_map(|extension| extension.tools)
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    registry_tools.sort();
    assert_eq!(registry_tools, ["newA", "newB"]);
    let surface = runner.resolve_command_surface("/workspace").await;
    let mut command_names = surface
        .commands
        .iter()
        .map(|command| command.command.name.as_str())
        .collect::<Vec<_>>();
    command_names.sort_unstable();
    let mut keybinding_commands = surface
        .ui
        .keybindings
        .iter()
        .map(|binding| binding.command.as_str())
        .collect::<Vec<_>>();
    keybinding_commands.sort_unstable();
    let mut status_text = surface
        .ui
        .status_items
        .iter()
        .map(|item| item.text.as_str())
        .collect::<Vec<_>>();
    status_text.sort_unstable();
    assert_eq!(command_names, ["newA", "newB"]);
    assert_eq!(keybinding_commands, command_names);
    assert_eq!(status_text, command_names);
}

#[tokio::test]
async fn sync_sources_reconciles_only_changed_sources_and_preserves_source_order() {
    fn counting(id: &'static str) -> (Arc<dyn Extension>, Arc<AtomicUsize>, Arc<AtomicUsize>) {
        let starts = Arc::new(AtomicUsize::new(0));
        let stops = Arc::new(AtomicUsize::new(0));
        (
            Arc::new(CountingExtension {
                id,
                starts: Arc::clone(&starts),
                stops: Arc::clone(&stops),
            }),
            starts,
            stops,
        )
    }

    let runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
    let ctx = ExtensionLoadContext {
        working_dir: None,
        host_router: None,
        transport_profile: Default::default(),
    };
    let (old_a, old_a_starts, old_a_stops) = counting("a");
    let (replacement_a, replacement_a_starts, replacement_a_stops) = counting("a");
    let (old_b, old_b_starts, old_b_stops) = counting("b");
    let (new_b, new_b_starts, new_b_stops) = counting("b");
    let (removed, removed_starts, removed_stops) = counting("removed");
    let (added, added_starts, added_stops) = counting("added");
    let initial_loads = Arc::new(AtomicUsize::new(0));
    let updated_loads = Arc::new(AtomicUsize::new(0));

    let initial = FingerprintSource {
        entries: vec![
            ("source:b", "v1", old_b),
            ("source:a", "v1", old_a),
            ("source:removed", "v1", removed),
        ],
        loads: Arc::clone(&initial_loads),
    };
    assert!(
        sync_extension_sources(&runner, &ctx, &[&initial])
            .await
            .is_empty()
    );

    let updated = FingerprintSource {
        entries: vec![
            ("source:a", "v1", replacement_a),
            ("source:b", "v2", new_b),
            ("source:added", "v1", added),
        ],
        loads: Arc::clone(&updated_loads),
    };
    assert!(
        sync_extension_sources(&runner, &ctx, &[&updated])
            .await
            .is_empty()
    );
    assert_eq!(
        sync_extension_sources(&runner, &ctx, &[&UnavailableFingerprintSource]).await,
        vec!["source unavailable"]
    );
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while old_b_stops.load(Ordering::SeqCst) == 0 || removed_stops.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("unpublished extension instances should retire");

    assert_eq!(old_a_starts.load(Ordering::SeqCst), 1);
    assert_eq!(old_a_stops.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_a_starts.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_a_stops.load(Ordering::SeqCst), 0);
    assert_eq!(initial_loads.load(Ordering::SeqCst), 3);
    assert_eq!(updated_loads.load(Ordering::SeqCst), 2);
    assert_eq!(old_b_starts.load(Ordering::SeqCst), 1);
    assert_eq!(old_b_stops.load(Ordering::SeqCst), 1);
    assert_eq!(new_b_starts.load(Ordering::SeqCst), 1);
    assert_eq!(new_b_stops.load(Ordering::SeqCst), 0);
    assert_eq!(removed_starts.load(Ordering::SeqCst), 1);
    assert_eq!(removed_stops.load(Ordering::SeqCst), 1);
    assert_eq!(added_starts.load(Ordering::SeqCst), 1);
    assert_eq!(added_stops.load(Ordering::SeqCst), 0);
    assert_eq!(
        runner.registered_extension_ids().await,
        vec!["a", "b", "added"]
    );
}

#[tokio::test]
async fn failed_candidate_batch_is_discarded_without_disturbing_the_committed_generation() {
    let runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
    let old_starts = Arc::new(AtomicUsize::new(0));
    let old_stops = Arc::new(AtomicUsize::new(0));
    let replacement_starts = Arc::new(AtomicUsize::new(0));
    let replacement_stops = Arc::new(AtomicUsize::new(0));
    let failing_starts = Arc::new(AtomicUsize::new(0));
    let failing_stops = Arc::new(AtomicUsize::new(0));
    let initial = FingerprintSource {
        entries: vec![(
            "source:stable",
            "v1",
            Arc::new(CountingExtension {
                id: "stable",
                starts: Arc::clone(&old_starts),
                stops: Arc::clone(&old_stops),
            }),
        )],
        loads: Arc::new(AtomicUsize::new(0)),
    };
    let ctx = ExtensionLoadContext {
        working_dir: None,
        host_router: None,
        transport_profile: Default::default(),
    };
    let errors = sync_extension_sources(&runner, &ctx, &[&initial]).await;
    assert!(errors.is_empty(), "initial sync failed: {errors:?}");
    let initial_generation = match runner.runtime_snapshot_state() {
        RuntimeSnapshotState::Stable(generation) => generation,
        RuntimeSnapshotState::Updating => unreachable!("completed sync must be stable"),
    };
    let candidate_loads = Arc::new(AtomicUsize::new(0));
    let candidate = FingerprintSource {
        entries: vec![
            (
                "source:stable",
                "v2",
                Arc::new(CountingExtension {
                    id: "stable",
                    starts: Arc::clone(&replacement_starts),
                    stops: Arc::clone(&replacement_stops),
                }),
            ),
            (
                "source:failing",
                "v1",
                Arc::new(StartFailingExtension {
                    id: "failing",
                    starts: Arc::clone(&failing_starts),
                    stops: Arc::clone(&failing_stops),
                }),
            ),
        ],
        loads: Arc::clone(&candidate_loads),
    };
    let errors = sync_extension_sources(&runner, &ctx, &[&candidate]).await;

    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("injected candidate startup failure"));
    assert_eq!(candidate_loads.load(Ordering::SeqCst), 2);
    assert_eq!(old_starts.load(Ordering::SeqCst), 1);
    assert_eq!(old_stops.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_starts.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_stops.load(Ordering::SeqCst), 1);
    assert_eq!(failing_starts.load(Ordering::SeqCst), 1);
    assert_eq!(failing_stops.load(Ordering::SeqCst), 1);
    assert_eq!(runner.registered_extension_ids().await, ["stable"]);
    assert_eq!(
        runner.runtime_snapshot_state(),
        RuntimeSnapshotState::Stable(initial_generation)
    );
    assert!(runner.shutdown().await.is_empty());
}
