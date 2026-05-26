//! Worker 侧调用宿主的抽象（可注入 mock）。

use async_trait::async_trait;
use serde_json::Value;

use crate::{
    runtime::{OutboundInvokeControl, Peer, PeerError},
    s5r::ErrorPayload,
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

impl HostClient {
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

#[cfg(test)]
mod host_tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{Value, json};

    use super::*;
    use crate::s5r::ErrorPayload;

    struct MockHost;

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
