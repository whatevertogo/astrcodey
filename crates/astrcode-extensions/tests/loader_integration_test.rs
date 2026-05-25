//! 集成测试：扩展加载器发现和解析清单文件。
//!
//! 测试扩展加载器在不存在的目录、空目录等边界条件下的行为，
//! 以及扩展运行时的工具注册和取出功能，判别值的往返转换。

use astrcode_core::extension::ExtensionManifest;
use astrcode_extensions::loader::{ExtensionLoader, WasmLimits};

/// 测试加载器在不存在的路径下返回空结果且不报错
#[tokio::test]
async fn loader_returns_empty_result_when_no_extensions_dir() {
    let limits = WasmLimits {
        fuel: 10_000_000,
        memory_bytes: 64 * 1024 * 1024,
    };
    let result = ExtensionLoader::load_all(Some("/nonexistent/path"), &limits).await;
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
    let result = ExtensionLoader::load_all(None, &limits).await;
    // 全局目录可能存在也可能不存在，但不应崩溃
    // 当 working_dir 为 None 时跳过项目目录扫描
    assert!(result.errors.is_empty());
}

/// 测试事件和模式的判别值能够正确往返转换
#[test]
fn discriminants_roundtrip() {
    use astrcode_core::extension::{ExtensionEvent, HookMode};
    use astrcode_extensions::wasm_api;

    // 事件判别值往返测试
    for event in [
        ExtensionEvent::SessionStart,
        ExtensionEvent::SessionResume,
        ExtensionEvent::SessionShutdown,
        ExtensionEvent::TurnStart,
        ExtensionEvent::TurnEnd,
        ExtensionEvent::PreToolUse,
        ExtensionEvent::PostToolUse,
        ExtensionEvent::BeforeProviderRequest,
        ExtensionEvent::AfterProviderResponse,
        ExtensionEvent::UserPromptSubmit,
    ] {
        let d = wasm_api::event_discriminant(event.clone());
        let back = wasm_api::event_from_discriminant(d);
        assert_eq!(back, Some(event));
    }

    // 模式判别值往返测试
    for mode in [
        HookMode::Blocking,
        HookMode::NonBlocking,
        HookMode::Advisory,
    ] {
        let d = wasm_api::mode_discriminant(mode);
        let back = wasm_api::mode_from_discriminant(d);
        assert_eq!(back, Some(mode));
    }
}

/// 测试清单解析丢弃未声明的 legacy 字段，仅保留显式定义的 metadata。
///
/// 历史上 manifest 还携带过 `subscriptions` / `tools` / `slash_commands` 字段；
/// 现在这些字段已迁移到 `extension_init` 中通过 host imports 注册。serde 默认
/// 忽略多余字段，所以老格式仍能反序列化（不报错），但这些字段不再有任何 runtime
/// 效果。
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
