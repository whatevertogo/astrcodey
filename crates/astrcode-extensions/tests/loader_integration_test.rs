//! 集成测试：扩展加载器边界条件与 manifest 解析。

use std::sync::Arc;

use astrcode_extension_sdk::extension::{ExtensionCapability, ExtensionManifest};
use astrcode_extensions::{
    loader::{
        ExtensionLoadContext, ExtensionLoadFailure, ExtensionLoader, ExtensionRuntime,
        ExtensionSource, LoadExtensionsResult,
    },
    runner::{ExtensionRunner, ExtensionStageStatus},
};

struct BrokenSource;

#[async_trait::async_trait]
impl ExtensionSource for BrokenSource {
    async fn load(&self, _ctx: &ExtensionLoadContext) -> LoadExtensionsResult {
        LoadExtensionsResult {
            extensions: Vec::new(),
            errors: vec!["broken extension failed".into()],
            load_failures: vec![ExtensionLoadFailure {
                extension_id: Some("broken-extension".into()),
                message: "broken extension failed".into(),
                duration_ms: None,
            }],
            load_success_durations: Default::default(),
        }
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
