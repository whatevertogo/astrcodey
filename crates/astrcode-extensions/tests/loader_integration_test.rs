//! 集成测试：扩展加载器发现和解析清单文件。
//!
//! 测试扩展加载器在不存在的目录、空目录等边界条件下的行为，
//! 以及扩展运行时的工具注册和取出功能，FFI 判别值的往返转换。

use astrcode_extensions::{loader::ExtensionLoader, runtime::ExtensionRuntime};

/// 测试加载器在不存在的路径下返回空结果且不报错
#[tokio::test]
async fn loader_returns_empty_result_when_no_extensions_dir() {
    let result = ExtensionLoader::load_all(Some("/nonexistent/path")).await;
    // 不应报错 — 仅返回空列表
    assert!(result.extensions.is_empty());
    assert!(result.errors.is_empty());
}

/// 测试加载器在 working_dir 为 None 时不崩溃
#[tokio::test]
async fn loader_returns_empty_result_for_none_working_dir() {
    let result = ExtensionLoader::load_all(None).await;
    // 全局目录可能存在也可能不存在，但不应崩溃
    // 当 working_dir 为 None 时跳过项目目录扫描
    assert!(result.errors.is_empty());
}

/// 测试运行时初始状态下没有待处理的工具
#[tokio::test]
async fn runtime_starts_with_empty_tools() {
    let runtime = ExtensionRuntime::new();
    assert!(runtime.take_pending_tools().is_empty());
}

/// 测试运行时的工具注册和取出功能
#[tokio::test]
async fn runtime_queues_and_flushes_tools() {
    let runtime = ExtensionRuntime::new();
    let def = astrcode_core::tool::ToolDefinition {
        name: "test_tool".into(),
        description: "test".into(),
        parameters: serde_json::json!({}),
        origin: astrcode_core::tool::ToolOrigin::Extension,
    };
    runtime.register_tool(def.clone());
    let tools = runtime.take_pending_tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "test_tool");
    // 取出后队列应为空
    assert!(runtime.take_pending_tools().is_empty());
}

/// 测试 FFI 事件和模式的判别值能够正确往返转换
#[test]
fn ffi_discriminants_roundtrip() {
    use astrcode_core::extension::{ExtensionEvent, HookMode};
    use astrcode_extensions::ffi;

    // 事件判别值往返测试
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

    // 模式判别值往返测试
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
