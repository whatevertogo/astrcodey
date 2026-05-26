//! 集成测试：扩展加载器边界条件与 manifest 解析。

use astrcode_extension_sdk::extension::{ExtensionCapability, ExtensionManifest};
use astrcode_extensions::loader::ExtensionLoader;

#[tokio::test]
async fn loader_returns_empty_result_when_no_extensions_dir() {
    let result = ExtensionLoader::load_all(Some("/nonexistent/path"), None).await;
    assert!(result.extensions.is_empty());
    assert!(result.errors.is_empty());
}

#[tokio::test]
async fn loader_returns_empty_result_for_none_working_dir() {
    let result = ExtensionLoader::load_all(None, None).await;
    assert!(result.errors.is_empty());
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
        "id": "stateful-test",
        "name": "Stateful Test",
        "capabilities": ["session_state", "emit_events"]
    }))
    .expect("manifest should parse capabilities");

    assert_eq!(
        manifest.capabilities,
        vec![
            ExtensionCapability::SessionState,
            ExtensionCapability::EmitEvents
        ]
    );
}
