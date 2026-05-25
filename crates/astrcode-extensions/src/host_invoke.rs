//! WASM `host_invoke` 宿主后端：能力实现与 manifest 权限表。
//!
//! - [`authorize`]：与 `ExtensionRunner` 的 `allows` 对称，按 manifest 声明的
//!   [`ExtensionCapability`] 校验 `host_invoke` 能力名。
//! - [`build_small_llm_invoker`]：server 注入的全局后端（不含 per-extension 校验）。

use std::{sync::Arc, time::Duration};

use astrcode_core::{
    extension::ExtensionCapability,
    llm::{LlmContent, LlmEvent, LlmMessage, LlmProvider, LlmRole},
};

use crate::wasm_api::HostInvoker;

const HOST_INVOKE_TIMEOUT: Duration = Duration::from_secs(30);

/// `host_invoke` 能力名 → 须在 manifest 中声明的 [`ExtensionCapability`]。
fn required_capability(cap: &str) -> Option<ExtensionCapability> {
    match cap {
        "small_llm.chat" => Some(ExtensionCapability::SmallModel),
        _ => None,
    }
}

/// manifest 是否允许调用该 `host_invoke` 能力名。
pub fn authorize(cap: &str, declared: &[ExtensionCapability]) -> Result<(), String> {
    let Some(required) = required_capability(cap) else {
        return Ok(());
    };
    if declared.contains(&required) {
        Ok(())
    } else {
        Err(format!(
            "permission denied: {} not declared",
            capability_wire_name(required)
        ))
    }
}

fn capability_wire_name(cap: ExtensionCapability) -> &'static str {
    match cap {
        ExtensionCapability::SessionState => "session_state",
        ExtensionCapability::SessionControl => "session_control",
        ExtensionCapability::SmallModel => "small_model",
        ExtensionCapability::SessionHistory => "session_history",
        ExtensionCapability::EmitEvents => "emit_events",
        ExtensionCapability::WorkspaceRead => "workspace_read",
        ExtensionCapability::ProcessSpawn => "process_spawn",
        ExtensionCapability::NetworkClient => "network_client",
    }
}

/// 成功响应 JSON：`{ "ok": true, "output": ... }`。
pub fn ok(output: serde_json::Value) -> String {
    serde_json::json!({ "ok": true, "output": output }).to_string()
}

/// 失败响应 JSON：`{ "ok": false, "error": "..." }`。
pub fn err(error: impl std::fmt::Display) -> String {
    serde_json::json!({ "ok": false, "error": error.to_string() }).to_string()
}

/// 构建 `small_llm.chat` 宿主后端。加载 WASM 后由 [`HostState::finish_manifest`] 绑定。
pub fn build_small_llm_invoker(small_llm: Arc<dyn LlmProvider>) -> HostInvoker {
    let handle = tokio::runtime::Handle::current();
    Arc::new(move |cap: &str, input: &str| -> String {
        match cap {
            "small_llm.chat" => {
                let provider = Arc::clone(&small_llm);
                let result = handle.block_on(async {
                    tokio::time::timeout(HOST_INVOKE_TIMEOUT, invoke_small_llm(&*provider, input))
                        .await
                        .map_err(|_| "small_llm.chat timed out".to_string())?
                });
                match result {
                    Ok(content) => ok(serde_json::json!({
                        "content": content,
                        "model": "small_llm"
                    })),
                    Err(e) => err(e),
                }
            },
            _ => err(format!("unknown capability: {cap}")),
        }
    })
}

async fn invoke_small_llm(provider: &dyn LlmProvider, input: &str) -> Result<String, String> {
    let req: serde_json::Value =
        serde_json::from_str(input).map_err(|e| format!("invalid input JSON: {e}"))?;

    let messages = req["messages"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|m| {
                    let role = match m["role"].as_str()? {
                        "user" => LlmRole::User,
                        "assistant" => LlmRole::Assistant,
                        "system" => LlmRole::System,
                        _ => LlmRole::User,
                    };
                    let content = m["content"].as_str().unwrap_or("").to_string();
                    Some(LlmMessage {
                        role,
                        content: vec![LlmContent::Text { text: content }],
                        name: None,
                        reasoning_content: None,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if messages.is_empty() {
        return Err("messages array is empty or missing".into());
    }

    let mut rx = provider
        .generate(messages, vec![])
        .await
        .map_err(|e| e.to_string())?;

    let mut text = String::new();
    while let Some(event) = rx.recv().await {
        match event {
            LlmEvent::ContentDelta { delta } => text.push_str(&delta),
            LlmEvent::Done { .. } => break,
            LlmEvent::Error { message } => return Err(message),
            _ => {},
        }
    }
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorize_denies_small_llm_without_declaration() {
        let err = authorize("small_llm.chat", &[]).unwrap_err();
        assert!(err.contains("small_model not declared"));
    }

    #[test]
    fn authorize_allows_small_llm_when_declared() {
        authorize("small_llm.chat", &[ExtensionCapability::SmallModel]).unwrap();
    }

    #[test]
    fn authorize_passes_unknown_cap_to_backend() {
        authorize("future.cap", &[ExtensionCapability::SmallModel]).unwrap();
    }
}
