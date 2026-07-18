//! Worker 侧调用宿主的抽象（可注入 mock）。

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    extension::{ExtensionHttpRequest, ExtensionHttpResponse},
    runtime::{OutboundInvokeControl, Peer, PeerError},
    s5r::ErrorPayload,
    session_inspect::{
        SessionInspectListOutput, SessionInspectProviderMessagesOutput,
        SessionInspectReadModelOutput, SessionInspectSnapshotOutput,
    },
};

/// 扩展子进程调用 `astrcode.*` 能力的接口。
#[async_trait]
pub trait HostApi: Send + Sync {
    async fn call(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload>;

    async fn call_stream(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload>;
}

pub(crate) struct PeerHostApi<T: crate::runtime::FrameTransport + 'static> {
    peer: std::sync::Arc<Peer<T>>,
    caller_extension_id: Option<String>,
}

impl<T: crate::runtime::FrameTransport + 'static> PeerHostApi<T> {
    pub fn new(peer: std::sync::Arc<Peer<T>>, caller_extension_id: impl Into<String>) -> Self {
        Self {
            peer,
            caller_extension_id: Some(caller_extension_id.into()),
        }
    }
}

#[async_trait]
impl<T> HostApi for PeerHostApi<T>
where
    T: crate::runtime::FrameTransport + Send + Sync + 'static,
{
    async fn call(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
        let caller = self.caller_extension_id.as_deref();
        self.peer
            .invoke(capability, input, caller, OutboundInvokeControl::default())
            .await
            .map_err(peer_error_to_payload)
    }

    async fn call_stream(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
        let caller = self.caller_extension_id.as_deref();
        self.peer
            .invoke_stream_collect(capability, input, caller)
            .await
            .map_err(peer_error_to_payload)
    }
}

fn peer_error_to_payload(err: PeerError) -> ErrorPayload {
    match err {
        PeerError::Closed => ErrorPayload::new("peer_closed", "host peer closed"),
        PeerError::Timeout => ErrorPayload::new("timeout", "host invoke timed out"),
        PeerError::Payload(msg) => ErrorPayload::new("host_error", msg),
        PeerError::Msg(msg) => ErrorPayload::new("transport_error", msg),
    }
}

static HOST_API: std::sync::OnceLock<std::sync::Arc<dyn HostApi>> = std::sync::OnceLock::new();

/// 在 `Worker::run_stdio` 启动前由运行时注入；测试可调用 [`inject_host_api`].
pub(crate) fn set_host_api(api: std::sync::Arc<dyn HostApi>) -> Result<(), ()> {
    HOST_API.set(api).map_err(|_| ())
}

/// 测试或自定义运行时注入 mock 宿主 API。
#[allow(clippy::result_unit_err)]
pub fn inject_host_api(api: std::sync::Arc<dyn HostApi>) -> Result<(), ()> {
    HOST_API.set(api).map_err(|_| ())
}

/// Worker 侧调用宿主能力（委托给已注入的 [`HostApi`]）。
pub struct HostClient;

/// `astrcode.process.spawn` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostProcessRequest {
    pub command: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stdin: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl HostProcessRequest {
    pub fn new(command: impl Into<String>) -> Self {
        Self {
            command: command.into(),
            args: Vec::new(),
            cwd: None,
            stdin: None,
            timeout_ms: None,
        }
    }
}

/// `astrcode.process.spawn` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostProcessOutput {
    pub status: Option<i32>,
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
    pub combined: String,
    pub stdout_truncated: bool,
    pub stderr_truncated: bool,
    pub combined_truncated: bool,
}

/// `astrcode.network.client` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostNetworkRequest {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub method: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

impl HostNetworkRequest {
    pub fn get(url: impl Into<String>) -> Self {
        Self {
            url: url.into(),
            method: None,
            headers: BTreeMap::new(),
            body: None,
            max_bytes: None,
            timeout_ms: None,
        }
    }
}

/// `astrcode.network.client` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostNetworkResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: String,
}

/// `astrcode.workspace.write` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceWriteRequest {
    pub path: String,
    pub content: String,
}

/// `astrcode.workspace.write` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceWriteOutput {
    pub path: String,
    pub bytes_written: usize,
    pub parent_created: bool,
}

/// `astrcode.workspace.edit` 的线缆请求。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceEditRequest {
    pub path: String,
    pub old_text: String,
    pub new_text: String,
    #[serde(default)]
    pub replace_all: bool,
}

/// `astrcode.workspace.edit` 的线缆响应。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceEditOutput {
    pub path: String,
    pub replacements: usize,
    pub bytes_written: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceListRequest {
    pub path: String,
    #[serde(default = "default_workspace_list_depth")]
    pub depth: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

const fn default_workspace_list_depth() -> usize {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceListEntry {
    pub name: String,
    pub path: String,
    pub kind: String,
    pub bytes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceListOutput {
    pub path: String,
    pub entries: Vec<HostWorkspaceListEntry>,
    pub returned_entries: usize,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceGrepRequest {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_matches: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_line_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceGrepMatch {
    pub path: String,
    pub line_number: usize,
    pub line: String,
    pub line_truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceGrepOutput {
    pub pattern: String,
    pub root: String,
    pub matches: Vec<HostWorkspaceGrepMatch>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceGlobRequest {
    pub pattern: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_matches: Option<usize>,
    #[serde(default)]
    pub include_ignored: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostWorkspaceGlobOutput {
    pub pattern: String,
    pub root: String,
    pub paths: Vec<String>,
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostSessionTargetRequest {
    pub target_session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostSessionInputRequest {
    pub target_session_id: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostSessionDeliveryOutput {
    pub status: String,
    #[serde(default)]
    pub turn_id: Option<String>,
    #[serde(default)]
    pub queue_len: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct HostSessionExecutionView {
    pub phase: String,
    pub active_turn_id: Option<String>,
    pub queued_inputs: usize,
}

impl HostClient {
    /// 调用宿主主模型（manifest 须声明 `main_model`）。
    pub async fn main_chat(messages: Value) -> Result<Value, ErrorPayload> {
        Self::call(
            "astrcode.llm.main_chat",
            serde_json::json!({ "messages": messages }),
        )
        .await
    }

    /// 调用宿主小模型（manifest 须声明 `small_model`）。
    pub async fn small_chat(messages: Value) -> Result<Value, ErrorPayload> {
        Self::call(
            "astrcode.llm.small_chat",
            serde_json::json!({ "messages": messages }),
        )
        .await
    }

    /// 运行受限子进程（manifest 须声明 `process_spawn`）。
    pub async fn spawn_process(
        request: HostProcessRequest,
    ) -> Result<HostProcessOutput, ErrorPayload> {
        let input = serialize_request(request)?;
        let output = Self::call("astrcode.process.spawn", input).await?;
        deserialize_response(output, "process.spawn")
    }

    /// 发起受限 HTTP 请求（manifest 须声明 `network_client`）。
    pub async fn network_request(
        request: HostNetworkRequest,
    ) -> Result<HostNetworkResponse, ErrorPayload> {
        let input = serialize_request(request)?;
        let output = Self::call("astrcode.network.client", input).await?;
        deserialize_response(output, "network.client")
    }

    /// 调用其他插件的公开 HTTP 路由（manifest 须声明 `public_http_dispatch`）。
    pub async fn dispatch_public_http(
        request: ExtensionHttpRequest,
    ) -> Result<ExtensionHttpResponse, ErrorPayload> {
        let input = serialize_request(request)?;
        let output = Self::call("astrcode.extension.http.public", input).await?;
        deserialize_response(output, "extension.http.public")
    }

    pub async fn inject_session_input(
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, ErrorPayload> {
        let output = Self::call(
            "astrcode.session.control.inject_or_start",
            serialize_request(request)?,
        )
        .await?;
        deserialize_response(output, "session.control.inject_or_start")
    }

    pub async fn interrupt_and_submit_session_input(
        request: HostSessionInputRequest,
    ) -> Result<HostSessionDeliveryOutput, ErrorPayload> {
        let output = Self::call(
            "astrcode.session.control.interrupt_and_submit",
            serialize_request(request)?,
        )
        .await?;
        deserialize_response(output, "session.control.interrupt_and_submit")
    }

    pub async fn cancel_session_turn(
        request: HostSessionTargetRequest,
    ) -> Result<(), ErrorPayload> {
        Self::call(
            "astrcode.session.control.cancel_turn",
            serialize_request(request)?,
        )
        .await
        .map(|_| ())
    }

    pub async fn session_execution_view(
        request: HostSessionTargetRequest,
    ) -> Result<HostSessionExecutionView, ErrorPayload> {
        let output = Self::call(
            "astrcode.session.control.execution_view",
            serialize_request(request)?,
        )
        .await?;
        deserialize_response(output, "session.control.execution_view")
    }

    /// 创建或替换工作区文件（manifest 须声明 `workspace_write`）。
    pub async fn write_workspace_file(
        request: HostWorkspaceWriteRequest,
    ) -> Result<HostWorkspaceWriteOutput, ErrorPayload> {
        let input = serialize_request(request)?;
        let output = Self::call("astrcode.workspace.write", input).await?;
        deserialize_response(output, "workspace.write")
    }

    /// 精确替换工作区文件片段（manifest 须声明 `workspace_write`）。
    pub async fn edit_workspace_file(
        request: HostWorkspaceEditRequest,
    ) -> Result<HostWorkspaceEditOutput, ErrorPayload> {
        let input = serialize_request(request)?;
        let output = Self::call("astrcode.workspace.edit", input).await?;
        deserialize_response(output, "workspace.edit")
    }

    pub async fn list_workspace(
        request: HostWorkspaceListRequest,
    ) -> Result<HostWorkspaceListOutput, ErrorPayload> {
        let output = Self::call("astrcode.workspace.list", serialize_request(request)?).await?;
        deserialize_response(output, "workspace.list")
    }

    pub async fn grep_workspace(
        request: HostWorkspaceGrepRequest,
    ) -> Result<HostWorkspaceGrepOutput, ErrorPayload> {
        let output = Self::call("astrcode.workspace.grep", serialize_request(request)?).await?;
        deserialize_response(output, "workspace.grep")
    }

    pub async fn glob_workspace(
        request: HostWorkspaceGlobRequest,
    ) -> Result<HostWorkspaceGlobOutput, ErrorPayload> {
        let output = Self::call("astrcode.workspace.glob", serialize_request(request)?).await?;
        deserialize_response(output, "workspace.glob")
    }

    /// 列出宿主可见的全部会话（manifest 须声明宿主级全局权限 `session_inspect`）。
    pub async fn list_sessions() -> Result<SessionInspectListOutput, ErrorPayload> {
        let output = Self::call("astrcode.session.inspect.list", serde_json::json!({})).await?;
        deserialize_response(output, "session.inspect.list")
    }

    /// 跨会话读取轻量快照（manifest 须声明宿主级全局权限 `session_inspect`）。
    pub async fn inspect_session_snapshot(
        session_id: &str,
    ) -> Result<SessionInspectSnapshotOutput, ErrorPayload> {
        let output = Self::call(
            "astrcode.session.inspect.snapshot",
            serde_json::json!({ "session_id": session_id }),
        )
        .await?;
        deserialize_response(output, "session.inspect.snapshot")
    }

    /// 跨会话读取稳定映射后的完整投影（manifest 须声明宿主级全局权限 `session_inspect`）。
    pub async fn inspect_session_read_model(
        session_id: &str,
    ) -> Result<SessionInspectReadModelOutput, ErrorPayload> {
        let output = Self::call(
            "astrcode.session.inspect.read_model",
            serde_json::json!({ "session_id": session_id }),
        )
        .await?;
        deserialize_response(output, "session.inspect.read_model")
    }

    /// 跨会话读取 provider 可见消息（manifest 须声明宿主级全局权限 `session_inspect`）。
    pub async fn inspect_provider_messages(
        session_id: &str,
    ) -> Result<SessionInspectProviderMessagesOutput, ErrorPayload> {
        let output = Self::call(
            "astrcode.session.inspect.provider_messages",
            serde_json::json!({ "session_id": session_id }),
        )
        .await?;
        deserialize_response(output, "session.inspect.provider_messages")
    }

    pub async fn call(capability: &str, input: Value) -> Result<Value, ErrorPayload> {
        let api = HOST_API
            .get()
            .ok_or_else(|| ErrorPayload::new("host_not_ready", "host peer not ready"))?;
        api.call(capability, input).await
    }

    pub async fn call_stream(capability: &str, input: Value) -> Result<Value, ErrorPayload> {
        let api = HOST_API
            .get()
            .ok_or_else(|| ErrorPayload::new("host_not_ready", "host peer not ready"))?;
        api.call_stream(capability, input).await
    }
}

fn serialize_request<T: Serialize>(request: T) -> Result<Value, ErrorPayload> {
    serde_json::to_value(request).map_err(|error| {
        ErrorPayload::new(
            "serialization_failed",
            format!("failed to serialize host request: {error}"),
        )
    })
}

fn deserialize_response<T: serde::de::DeserializeOwned>(
    output: Value,
    capability: &str,
) -> Result<T, ErrorPayload> {
    serde_json::from_value(output).map_err(|error| {
        ErrorPayload::new(
            "invalid_host_response",
            format!("invalid {capability} response: {error}"),
        )
    })
}

#[cfg(test)]
mod host_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::s5r::ErrorPayload;

    struct MockHost;

    #[test]
    fn bounded_io_contracts_match_wire_shape() {
        let mut process = HostProcessRequest::new("rustc");
        process.args.push("--version".into());
        process.timeout_ms = Some(1_000);
        let value = serialize_request(process).expect("serialize process request");
        assert_eq!(value["command"], "rustc");
        assert_eq!(value["args"], json!(["--version"]));
        assert_eq!(value["timeout_ms"], 1_000);

        let response = deserialize_response::<HostNetworkResponse>(
            json!({ "status": 200, "headers": {}, "body": "ok" }),
            "network.client",
        )
        .expect("deserialize network response");
        assert_eq!(response.status, 200);
        assert_eq!(response.body, "ok");
    }

    #[async_trait]
    impl HostApi for MockHost {
        async fn call(&self, capability: &str, _input: Value) -> Result<Value, ErrorPayload> {
            Ok(json!({ "capability": capability }))
        }

        async fn call_stream(&self, capability: &str, input: Value) -> Result<Value, ErrorPayload> {
            self.call(capability, input).await
        }
    }

    #[tokio::test]
    async fn inject_host_api_allows_host_client_call() {
        let _ = inject_host_api(Arc::new(MockHost));
        let out = HostClient::call("astrcode.test", json!({})).await.unwrap();
        assert_eq!(out["capability"], "astrcode.test");
    }
}
