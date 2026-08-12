//! 集成测试：扩展加载器边界条件与 manifest 解析。

use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use astrcode_extension_sdk::{
    builder::{
        command, command_handler, http_handler, http_route, keybinding, manifest, status_item,
        tool, tool_handler,
    },
    extension::{
        Extension, ExtensionCall, ExtensionCapability, ExtensionCommandResult, ExtensionError,
        ExtensionHttpMethod, ExtensionHttpRequest, ExtensionHttpResponse, ExtensionManifest,
        ExtensionStartContext, Registrar,
    },
    runtime_ports::{RuntimeSnapshotProvider, RuntimeSnapshotState},
    tool::ToolResult,
};
use astrcode_extensions::{
    loader::{
        DiscoverExtensionsResult, ExtensionCandidate, ExtensionLoadContext, ExtensionLoadFailure,
        ExtensionSource, sync_extension_sources,
    },
    runner::{ExtensionHttpDispatchResult, ExtensionRunner, ExtensionStageStatus},
};
use tokio::sync::Notify;

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
            tool_handler(|_| async {
                Ok(ToolResult::text("ok".into(), false, Default::default()))
            }),
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

struct RetirementControlledExtension {
    id: &'static str,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
    stop_entered: Arc<Notify>,
    stop_release: Option<Arc<Notify>>,
    stop_error: Option<&'static str>,
}

#[async_trait::async_trait]
impl Extension for RetirementControlledExtension {
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
        self.stop_entered.notify_one();
        if let Some(stop_release) = &self.stop_release {
            stop_release.notified().await;
        }
        self.stops.fetch_add(1, Ordering::SeqCst);
        match self.stop_error {
            Some(message) => Err(ExtensionError::Internal(message.into())),
            None => Ok(()),
        }
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

    fn owns_source_key(&self, source_key: &str) -> bool {
        source_key.starts_with("source:")
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
        },
        &[&source],
    )
    .await;

    assert!(errors.is_empty());
    assert_eq!(loads.load(Ordering::SeqCst), 0);
    assert!(runner.registered_extension_ids().await.is_empty());
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
        RuntimeSnapshotState::Updating
    );

    let mut concurrent_reader = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { published_tool_catalog(&runner).await })
    };
    let mut concurrent_http_reader = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { published_http_tool(&runner, "/catalog/catalog-b").await })
    };
    let mut concurrent_registry_reader = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.registry_snapshot().await })
    };
    let mut concurrent_command_surface = {
        let runner = Arc::clone(&runner);
        tokio::spawn(async move { runner.resolve_command_surface("/workspace").await })
    };
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut concurrent_reader,)
            .await
            .is_err(),
        "an extension view must not expose a partially replaced tool catalog"
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            &mut concurrent_http_reader,
        )
        .await
        .is_err(),
        "HTTP dispatch must not expose a partially replaced route catalog"
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            &mut concurrent_registry_reader,
        )
        .await
        .is_err(),
        "registry snapshots must not expose partially replaced declarations"
    );
    assert!(
        tokio::time::timeout(
            std::time::Duration::from_millis(50),
            &mut concurrent_command_surface,
        )
        .await
        .is_err(),
        "command and UI contributions must wait for one stable generation"
    );

    start_release.notify_one();
    assert!(reload.await.unwrap().is_empty());
    let (final_generation, final_names) = concurrent_reader.await.unwrap();
    assert_eq!(final_names, ["newA", "newB"]);
    assert!(final_generation > initial_generation);
    assert_eq!(
        runner.runtime_snapshot_state(),
        RuntimeSnapshotState::Stable(final_generation)
    );
    assert_eq!(concurrent_http_reader.await.unwrap(), "newB");
    let registry = concurrent_registry_reader.await.unwrap();
    let mut registry_tools = registry
        .extensions
        .into_iter()
        .flat_map(|extension| extension.tools)
        .map(|tool| tool.name)
        .collect::<Vec<_>>();
    registry_tools.sort();
    assert_eq!(registry_tools, ["newA", "newB"]);
    let surface = concurrent_command_surface.await.unwrap();
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
async fn sync_sources_waits_for_renamed_retirement_and_blocks_failed_replacement() {
    let runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
    let old_starts = Arc::new(AtomicUsize::new(0));
    let old_stops = Arc::new(AtomicUsize::new(0));
    let stop_entered = Arc::new(Notify::new());
    let stop_release = Arc::new(Notify::new());
    let new_starts = Arc::new(AtomicUsize::new(0));
    let new_stops = Arc::new(AtomicUsize::new(0));
    let replacement_loads = Arc::new(AtomicUsize::new(0));
    let initial = FingerprintSource {
        entries: vec![(
            "source:renamed",
            "v1",
            Arc::new(RetirementControlledExtension {
                id: "rename-v1",
                starts: Arc::clone(&old_starts),
                stops: Arc::clone(&old_stops),
                stop_entered: Arc::clone(&stop_entered),
                stop_release: Some(Arc::clone(&stop_release)),
                stop_error: None,
            }),
        )],
        loads: Arc::new(AtomicUsize::new(0)),
    };
    let ctx = ExtensionLoadContext {
        working_dir: None,
        host_router: None,
    };
    let errors = sync_extension_sources(&runner, &ctx, &[&initial]).await;
    assert!(errors.is_empty(), "initial sync failed: {errors:?}");

    let mut replacement = {
        let runner = Arc::clone(&runner);
        let new_starts = Arc::clone(&new_starts);
        let new_stops = Arc::clone(&new_stops);
        let replacement_loads = Arc::clone(&replacement_loads);
        tokio::spawn(async move {
            let updated = FingerprintSource {
                entries: vec![(
                    "source:renamed",
                    "v2",
                    Arc::new(CountingExtension {
                        id: "rename-v2",
                        starts: new_starts,
                        stops: new_stops,
                    }),
                )],
                loads: replacement_loads,
            };
            sync_extension_sources(
                &runner,
                &ExtensionLoadContext {
                    working_dir: None,
                    host_router: None,
                },
                &[&updated],
            )
            .await
        })
    };
    tokio::time::timeout(std::time::Duration::from_secs(1), stop_entered.notified())
        .await
        .expect("renamed source should begin retiring its previous extension id");
    assert_eq!(replacement_loads.load(Ordering::SeqCst), 0);
    assert_eq!(new_starts.load(Ordering::SeqCst), 0);
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(50), &mut replacement)
            .await
            .is_err(),
        "the renamed extension must not start before the old id finishes retirement"
    );

    stop_release.notify_one();
    assert!(
        tokio::time::timeout(std::time::Duration::from_secs(1), &mut replacement)
            .await
            .expect("renamed replacement should resume after retirement")
            .unwrap()
            .is_empty()
    );
    assert_eq!(old_starts.load(Ordering::SeqCst), 1);
    assert_eq!(old_stops.load(Ordering::SeqCst), 1);
    assert_eq!(replacement_loads.load(Ordering::SeqCst), 1);
    assert_eq!(new_starts.load(Ordering::SeqCst), 1);
    assert_eq!(runner.registered_extension_ids().await, ["rename-v2"]);
    assert!(runner.shutdown().await.is_empty());

    let runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
    let failed_starts = Arc::new(AtomicUsize::new(0));
    let failed_stops = Arc::new(AtomicUsize::new(0));
    let replacement_starts = Arc::new(AtomicUsize::new(0));
    let failed_replacement_loads = Arc::new(AtomicUsize::new(0));
    let initial = FingerprintSource {
        entries: vec![(
            "source:failed",
            "v1",
            Arc::new(RetirementControlledExtension {
                id: "failed-v1",
                starts: Arc::clone(&failed_starts),
                stops: Arc::clone(&failed_stops),
                stop_entered: Arc::new(Notify::new()),
                stop_release: None,
                stop_error: Some("injected retirement failure"),
            }),
        )],
        loads: Arc::new(AtomicUsize::new(0)),
    };
    assert!(
        sync_extension_sources(&runner, &ctx, &[&initial])
            .await
            .is_empty()
    );
    let updated = FingerprintSource {
        entries: vec![(
            "source:failed",
            "v2",
            Arc::new(CountingExtension {
                id: "failed-v2",
                starts: Arc::clone(&replacement_starts),
                stops: Arc::new(AtomicUsize::new(0)),
            }),
        )],
        loads: Arc::clone(&failed_replacement_loads),
    };
    let errors = sync_extension_sources(&runner, &ctx, &[&updated]).await;

    assert_eq!(failed_starts.load(Ordering::SeqCst), 1);
    assert_eq!(failed_stops.load(Ordering::SeqCst), 1);
    assert_eq!(failed_replacement_loads.load(Ordering::SeqCst), 0);
    assert_eq!(replacement_starts.load(Ordering::SeqCst), 0);
    assert!(runner.registered_extension_ids().await.is_empty());
    assert_eq!(errors.len(), 1);
    assert!(errors[0].contains("failed to retire extension failed-v1"));
    assert!(errors[0].contains("before starting failed-v2"));
    assert!(errors[0].contains("injected retirement failure"));
    assert!(runner.shutdown().await.is_empty());
}
