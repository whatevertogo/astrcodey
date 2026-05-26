//! 集成测试：扩展加载器发现和解析清单文件。
//!
//! 测试扩展加载器在不存在的目录、空目录等边界条件下的行为，
//! 以及扩展运行时的工具注册和取出功能，判别值的往返转换。

use astrcode_extension_sdk::extension::{ExtensionCapability, ExtensionManifest};
use astrcode_extensions::loader::{ExtensionLoader, WasmLimits};

/// 测试加载器在不存在的路径下返回空结果且不报错
#[tokio::test]
async fn loader_returns_empty_result_when_no_extensions_dir() {
    let limits = WasmLimits {
        fuel: 10_000_000,
        memory_bytes: 64 * 1024 * 1024,
    };
    let result = ExtensionLoader::load_all(Some("/nonexistent/path"), &limits, None).await;
    // 不应报错 — 仅返回空列表
    assert!(result.extensions.is_empty());
    assert!(result.errors.is_empty());
}

/// 测试加载器在 working_dir 为 None 时不崩溃
#[tokio::test]
async fn loader_returns_empty_result_for_none_working_dir() {
    let limits = WasmLimits {
        fuel: 10_000_000,
        memory_bytes: 64 * 1024 * 1024,
    };
    let result = ExtensionLoader::load_all(None, &limits, None).await;
    // 全局目录可能存在也可能不存在，但不应崩溃
    // 当 working_dir 为 None 时跳过项目目录扫描
    assert!(result.errors.is_empty());
}

/// 测试 s5r 事件名和模式名能正确解析为 Rust 类型。
#[test]
fn s5r_event_and_mode_names_roundtrip() {
    use astrcode_extension_sdk::{
        extension::{ExtensionEvent, HookMode},
        s5r::{event_from_name, mode_from_name},
    };

    let cases: &[(&str, ExtensionEvent)] = &[
        ("session_start", ExtensionEvent::SessionStart),
        ("session_resume", ExtensionEvent::SessionResume),
        ("session_shutdown", ExtensionEvent::SessionShutdown),
        ("turn_start", ExtensionEvent::TurnStart),
        ("turn_end", ExtensionEvent::TurnEnd),
        ("pre_tool_use", ExtensionEvent::PreToolUse),
        ("post_tool_use", ExtensionEvent::PostToolUse),
        (
            "before_provider_request",
            ExtensionEvent::BeforeProviderRequest,
        ),
        (
            "after_provider_response",
            ExtensionEvent::AfterProviderResponse,
        ),
        ("user_prompt_submit", ExtensionEvent::UserPromptSubmit),
        ("prompt_build", ExtensionEvent::PromptBuild),
        ("pre_compact", ExtensionEvent::PreCompact),
        ("post_compact", ExtensionEvent::PostCompact),
    ];
    for (name, expected) in cases {
        assert_eq!(
            event_from_name(name),
            Some(expected.clone()),
            "event name: {name}"
        );
    }
    assert!(event_from_name("nonexistent_event").is_none());

    assert_eq!(mode_from_name("blocking"), Some(HookMode::Blocking));
    assert_eq!(mode_from_name("non_blocking"), Some(HookMode::NonBlocking));
    assert_eq!(mode_from_name("advisory"), Some(HookMode::Advisory));
    assert!(mode_from_name("unknown_mode").is_none());
}

/// 测试 extension.json 清单解析丢弃未知字段。
///
/// **Loader 仅使用 `library`**：tools/commands/hooks/capabilities 由 WASM
/// `extension_init` 握手返回；serde 仍解析其它字段但不参与加载。磁盘路径仅 WASM。
#[test]
fn manifest_ignores_legacy_capability_fields() {
    let manifest: ExtensionManifest = serde_json::from_value(serde_json::json!({
        "id": "legacy-test",
        "name": "Legacy Test",
        "library": "legacy-test.wasm",
        // 老字段：当前实现不再读取，但不应导致解析失败。
        "subscriptions": [
            { "event": "pre_tool_use", "mode": "blocking", "priority": 10 }
        ],
        "tools": [],
        "slash_commands": []
    }))
    .expect("manifest should deserialize, ignoring legacy fields");

    assert_eq!(manifest.id, "legacy-test");
    assert_eq!(manifest.library, "legacy-test.wasm");
}

#[test]
fn manifest_declares_requested_host_capabilities() {
    let manifest: ExtensionManifest = serde_json::from_value(serde_json::json!({
        "id": "stateful-test",
        "name": "Stateful Test",
        "library": "stateful-test.wasm",
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
