//! 集成测试：扩展加载器边界条件与 manifest 解析。

use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

use astrcode_extension_sdk::extension::{
    Extension, ExtensionCapability, ExtensionCtx, ExtensionError, ExtensionManifest,
};
use astrcode_extensions::{
    loader::{
        DiscoverExtensionsResult, ExtensionCandidate, ExtensionLoadContext, ExtensionLoadFailure,
        ExtensionLoader, ExtensionRuntime, ExtensionSource,
    },
    runner::{ExtensionRunner, ExtensionStageStatus},
};
use tokio::sync::Notify;

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

#[async_trait::async_trait]
impl ExtensionSource for BrokenSource {
    async fn discover(&self, _ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult {
        DiscoverExtensionsResult {
            candidates: Vec::new(),
            errors: vec!["broken extension failed".into()],
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
                let id = extension.id();
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
    fn id(&self) -> &str {
        "batch-observer"
    }

    async fn start(&self, ctx: ExtensionCtx) -> Result<(), ExtensionError> {
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
    fn id(&self) -> &str {
        self.0
    }
}

struct CountingExtension {
    id: &'static str,
    starts: Arc<AtomicUsize>,
    stops: Arc<AtomicUsize>,
}

#[async_trait::async_trait]
impl Extension for CountingExtension {
    fn id(&self) -> &str {
        self.id
    }

    async fn start(&self, _ctx: ExtensionCtx) -> Result<(), ExtensionError> {
        self.starts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }

    async fn stop(
        &self,
        _reason: astrcode_extension_sdk::extension::StopReason,
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

#[async_trait::async_trait]
impl ExtensionSource for FingerprintSource {
    async fn discover(&self, _ctx: &ExtensionLoadContext) -> DiscoverExtensionsResult {
        DiscoverExtensionsResult {
            candidates: self
                .entries
                .iter()
                .map(|(source_key, fingerprint, extension)| {
                    let extension_id = extension.id().to_string();
                    let extension = Arc::clone(extension);
                    let loads = Arc::clone(&self.loads);
                    ExtensionCandidate::lazy(
                        *source_key,
                        *fingerprint,
                        Some(extension_id),
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
            errors: vec!["source unavailable".into()],
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

struct IsolatedTestHome {
    _temp: tempfile::TempDir,
    prev: Option<String>,
}

impl IsolatedTestHome {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("tempdir");
        let prev = std::env::var("ASTRCODE_TEST_HOME").ok();
        std::env::set_var("ASTRCODE_TEST_HOME", temp.path());
        Self { _temp: temp, prev }
    }
}

impl Drop for IsolatedTestHome {
    fn drop(&mut self) {
        match &self.prev {
            Some(value) => std::env::set_var("ASTRCODE_TEST_HOME", value),
            None => std::env::remove_var("ASTRCODE_TEST_HOME"),
        }
    }
}

#[tokio::test]
async fn loader_returns_empty_result_when_no_extensions_dir() {
    let _home = IsolatedTestHome::new();
    let result = ExtensionLoader::load_all(Some("/nonexistent/path"), None).await;
    assert!(result.extensions.is_empty());
    assert!(result.errors.is_empty());
}

#[tokio::test]
async fn loader_returns_empty_result_for_none_working_dir() {
    let _home = IsolatedTestHome::new();
    let result = ExtensionLoader::load_all(None, None).await;
    assert!(result.extensions.is_empty());
    assert!(result.errors.is_empty());
}

#[tokio::test]
async fn sync_sources_records_load_failure_diagnostics() {
    let runner = Arc::new(ExtensionRunner::new(std::time::Duration::from_secs(1)));
    let source = BrokenSource;
    let errors = ExtensionRuntime::sync_sources(
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

    let errors = ExtensionRuntime::sync_sources(
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
        ExtensionRuntime::sync_sources(&runner, &ctx, &[&initial])
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
        ExtensionRuntime::sync_sources(&runner, &ctx, &[&updated])
            .await
            .is_empty()
    );
    assert_eq!(
        ExtensionRuntime::sync_sources(&runner, &ctx, &[&UnavailableFingerprintSource]).await,
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

#[test]
fn s5r_event_and_mode_names_roundtrip() {
    use astrcode_extension_sdk::{
        extension::{ExtensionEvent, HookMode},
        s5r::{event_from_name, mode_from_name},
    };

    let cases: &[(&str, ExtensionEvent)] = &[
        ("session_start", ExtensionEvent::SessionStart),
        ("pre_tool_use", ExtensionEvent::PreToolUse),
        ("turn_end", ExtensionEvent::TurnEnd),
    ];
    for (name, expected) in cases {
        assert_eq!(event_from_name(name), Some(expected.clone()));
    }
    assert_eq!(mode_from_name("blocking"), Some(HookMode::Blocking));
}

#[test]
fn manifest_deserializes_with_extra_legacy_fields() {
    // 旧版 extension.json 可能含 `library` 等已删除字段；serde 默认忽略未知字段以保持兼容。
    let manifest: ExtensionManifest = serde_json::from_value(serde_json::json!({
        "id": "legacy-test",
        "name": "Legacy Test",
        "library": "ignored",
        "tools": [],
    }))
    .expect("manifest should deserialize");

    assert_eq!(manifest.id, "legacy-test");
}

#[test]
fn manifest_declares_requested_host_capabilities() {
    let manifest: ExtensionManifest = serde_json::from_value(serde_json::json!({
        "id": "eventful-test",
        "name": "Eventful Test",
        "capabilities": ["emit_events"]
    }))
    .expect("manifest should parse capabilities");

    assert_eq!(manifest.capabilities, vec![ExtensionCapability::EmitEvents]);
}
