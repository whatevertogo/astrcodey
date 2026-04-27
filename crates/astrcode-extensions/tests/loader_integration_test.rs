//! Integration test: extension loader discovers and parses manifests.

use astrcode_extensions::{loader::ExtensionLoader, runtime::ExtensionRuntime};

#[tokio::test]
async fn loader_returns_empty_result_when_no_extensions_dir() {
    let result = ExtensionLoader::load_all(Some("/nonexistent/path")).await;
    // Should not error — just return empty
    assert!(result.extensions.is_empty());
    assert!(result.errors.is_empty());
}

#[tokio::test]
async fn loader_returns_empty_result_for_none_working_dir() {
    let result = ExtensionLoader::load_all(None).await;
    // Global dir may or may not exist, but shouldn't crash
    // Project dir skipped when None
    assert!(result.errors.is_empty());
}

#[tokio::test]
async fn runtime_starts_with_empty_tools() {
    let runtime = ExtensionRuntime::new();
    assert!(runtime.take_pending_tools().is_empty());
}

#[tokio::test]
async fn runtime_queues_and_flushes_tools() {
    let runtime = ExtensionRuntime::new();
    let def = astrcode_core::tool::ToolDefinition {
        name: "test_tool".into(),
        description: "test".into(),
        parameters: serde_json::json!({}),
        is_builtin: false,
    };
    runtime.register_tool(def.clone());
    let tools = runtime.take_pending_tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "test_tool");
    // After take, queue is empty
    assert!(runtime.take_pending_tools().is_empty());
}

#[test]
fn ffi_discriminants_roundtrip() {
    use astrcode_core::extension::{ExtensionEvent, HookMode};
    use astrcode_extensions::ffi;

    // Event discriminants roundtrip
    for event in [
        ExtensionEvent::SessionStart,
        ExtensionEvent::SessionShutdown,
        ExtensionEvent::TurnStart,
        ExtensionEvent::TurnEnd,
        ExtensionEvent::PreToolUse,
        ExtensionEvent::PostToolUse,
        ExtensionEvent::BeforeProviderRequest,
        ExtensionEvent::AfterProviderResponse,
        ExtensionEvent::UserPromptSubmit,
    ] {
        let d = ffi::event_discriminant(event.clone());
        let back = ffi::event_from_discriminant(d);
        assert_eq!(back, Some(event));
    }

    // Mode discriminants roundtrip
    for mode in [
        HookMode::Blocking,
        HookMode::NonBlocking,
        HookMode::Advisory,
    ] {
        let d = ffi::mode_discriminant(mode);
        let back = ffi::mode_from_discriminant(d);
        assert_eq!(back, Some(mode));
    }
}
