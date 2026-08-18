use std::{collections::BTreeSet, sync::Arc};

use astrcode_extension_sdk::wire::{
    WireErrorCode,
    frame::{FrameError, FrameTransport},
    manifest::InitializeManifest,
    protocol::{
        ActivateMsg, ActivateOutput, ErrorPayload, FeatureName, InitializeMsg, InitializeOutput,
        PeerInfo, ProtocolError, ResultKind, ResultMsg, S5R_VERSION, WireMessage,
        encode_wire_message, negotiate_features, parse_wire_message,
    },
};

use crate::frame::{read_traced_frame, write_traced_frame};

pub struct Uninitialized {
    local_peer: PeerInfo,
}

pub struct HostInitialized {
    negotiated_features: BTreeSet<FeatureName>,
}

pub struct WorkerInitialized {
    negotiated_features: BTreeSet<FeatureName>,
    host_operations: Vec<String>,
}

pub struct Ready {
    pub(crate) negotiated_features: BTreeSet<FeatureName>,
}

/// Worker-side ready state: the sole owner of the host operation catalog
/// received during the handshake. The host side never retains a copy.
pub struct WorkerReady {
    negotiated_features: BTreeSet<FeatureName>,
    host_operations: Vec<String>,
}

pub struct HostInitialization {
    pub request_id: String,
    pub extension_id: String,
    pub supported_features: BTreeSet<FeatureName>,
    pub required_features: BTreeSet<FeatureName>,
    pub host_operations: Vec<String>,
}

impl HostInitialization {
    pub fn new(request_id: impl Into<String>, extension_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            extension_id: extension_id.into(),
            supported_features: BTreeSet::new(),
            required_features: BTreeSet::new(),
            host_operations: Vec::new(),
        }
    }
}

pub struct WorkerInitialization {
    pub supported_features: BTreeSet<FeatureName>,
    pub required_features: BTreeSet<FeatureName>,
    pub manifest: InitializeManifest,
}

impl WorkerInitialization {
    pub fn new(manifest: InitializeManifest) -> Self {
        Self {
            supported_features: BTreeSet::new(),
            required_features: BTreeSet::new(),
            manifest,
        }
    }
}

/// An S5R peer whose callable surface is determined by its handshake state.
///
/// ```compile_fail
/// use astrcode_extension_sdk::wire::{HostInitialized, Peer, ProcessStdioTransport};
///
/// fn start_runtime(peer: Peer<ProcessStdioTransport, HostInitialized>) {
///     peer.into_runtime();
/// }
/// ```
pub struct Peer<T, State = Uninitialized> {
    transport: Arc<T>,
    state: State,
}

impl<T> Peer<T, Uninitialized>
where
    T: FrameTransport + 'static,
{
    pub fn new(transport: T, local_peer: PeerInfo) -> Self {
        Self {
            transport: Arc::new(transport),
            state: Uninitialized { local_peer },
        }
    }

    pub async fn initialize(
        self,
        initialization: HostInitialization,
    ) -> Result<(Peer<T, HostInitialized>, PeerInfo, InitializeManifest), PeerError> {
        self.state
            .local_peer
            .validate()
            .map_err(|error| PeerError::Protocol(error.to_string()))?;
        validate_host_initialization(&initialization)?;
        let initialize = InitializeMsg {
            id: initialization.request_id.clone(),
            protocol_version: S5R_VERSION.into(),
            host: self.state.local_peer.clone(),
            extension_id: initialization.extension_id.clone(),
            supported_features: initialization.supported_features.iter().cloned().collect(),
            required_features: initialization.required_features.iter().cloned().collect(),
            host_operations: initialization.host_operations,
        };
        self.write(&WireMessage::Initialize(initialize)).await?;

        let WireMessage::Result(result) = self.read().await? else {
            return Err(PeerError::UnexpectedMessage("initialize result"));
        };
        if result.id() != initialization.request_id || result.kind() != ResultKind::Initialize {
            return Err(PeerError::UnexpectedMessage("matching initialize result"));
        }
        let output = match result {
            ResultMsg::Success { output, .. } => output,
            ResultMsg::Failure { error, .. } => return Err(PeerError::Remote(error)),
        };
        let output: InitializeOutput = serde_json::from_value(output)
            .map_err(|error| PeerError::Protocol(error.to_string()))?;
        validate_initialize_output(
            &output,
            &initialization.extension_id,
            &initialization.supported_features,
            &initialization.required_features,
        )?;
        Ok((
            Peer {
                transport: self.transport,
                state: HostInitialized {
                    negotiated_features: output.negotiated_features.into_iter().collect(),
                },
            },
            output.worker,
            output.manifest,
        ))
    }

    pub async fn accept(
        self,
        initialization: WorkerInitialization,
    ) -> Result<Peer<T, WorkerInitialized>, PeerError> {
        self.state
            .local_peer
            .validate()
            .map_err(|error| PeerError::Protocol(error.to_string()))?;
        validate_feature_declaration(
            &initialization.supported_features,
            &initialization.required_features,
        )?;
        let WireMessage::Initialize(initialize) = self.read().await? else {
            return Err(PeerError::UnexpectedMessage("initialize request"));
        };
        let result = validate_initialize(
            &initialize,
            &self.state.local_peer,
            &initialization.supported_features,
            &initialization.required_features,
        );
        match result {
            Ok(negotiated) => {
                let output = InitializeOutput {
                    worker: self.state.local_peer.clone(),
                    protocol_version: S5R_VERSION.into(),
                    supported_features: initialization.supported_features.iter().cloned().collect(),
                    required_features: initialization.required_features.iter().cloned().collect(),
                    negotiated_features: negotiated.iter().cloned().collect(),
                    manifest: initialization.manifest,
                };
                self.write(&WireMessage::Result(ResultMsg::success(
                    initialize.id,
                    ResultKind::Initialize,
                    serde_json::to_value(output)?,
                )))
                .await?;
                Ok(Peer {
                    transport: self.transport,
                    state: WorkerInitialized {
                        negotiated_features: negotiated,
                        host_operations: initialize.host_operations,
                    },
                })
            },
            Err(error) => {
                self.write(&WireMessage::Result(ResultMsg::failure(
                    initialize.id,
                    ResultKind::Initialize,
                    error.clone(),
                )))
                .await?;
                Err(PeerError::Remote(error))
            },
        }
    }
}

impl<T> Peer<T, HostInitialized>
where
    T: FrameTransport + 'static,
{
    pub fn negotiated_features(&self) -> &BTreeSet<FeatureName> {
        &self.state.negotiated_features
    }

    pub async fn activate(
        self,
        request_id: impl Into<String>,
        config: serde_json::Value,
    ) -> Result<Peer<T, Ready>, PeerError> {
        let request_id = request_id.into();
        if request_id.is_empty() {
            return Err(PeerError::Protocol(
                "activate request id must not be empty".into(),
            ));
        }
        self.write(&WireMessage::Activate(ActivateMsg {
            id: request_id.clone(),
            config,
        }))
        .await?;
        let WireMessage::Result(result) = self.read().await? else {
            return Err(PeerError::UnexpectedMessage("activate result"));
        };
        if result.id() != request_id || result.kind() != ResultKind::Activate {
            return Err(PeerError::UnexpectedMessage("matching activate result"));
        }
        let output = match result {
            ResultMsg::Success { output, .. } => output,
            ResultMsg::Failure { error, .. } => return Err(PeerError::Remote(error)),
        };
        serde_json::from_value::<ActivateOutput>(output)
            .map_err(|error| PeerError::Protocol(error.to_string()))?;
        let Peer { transport, state } = self;
        Ok(Peer {
            transport,
            state: Ready {
                negotiated_features: state.negotiated_features,
            },
        })
    }
}

impl<T> Peer<T, WorkerInitialized>
where
    T: FrameTransport + 'static,
{
    pub async fn accept_activation<F, Fut>(
        self,
        handler: F,
    ) -> Result<Peer<T, WorkerReady>, PeerError>
    where
        F: FnOnce(serde_json::Value) -> Fut,
        Fut: std::future::Future<Output = Result<(), ErrorPayload>>,
    {
        let WireMessage::Activate(activation) = self.read().await? else {
            return Err(PeerError::UnexpectedMessage("activate request"));
        };
        if activation.id.is_empty() {
            let error = ErrorPayload::new(
                WireErrorCode::InvalidRequest,
                "activate request id must not be empty",
            );
            self.write(&WireMessage::Result(ResultMsg::failure(
                activation.id,
                ResultKind::Activate,
                error.clone(),
            )))
            .await?;
            return Err(PeerError::Remote(error));
        }
        if let Err(error) = handler(activation.config).await {
            self.write(&WireMessage::Result(ResultMsg::failure(
                activation.id,
                ResultKind::Activate,
                error.clone(),
            )))
            .await?;
            return Err(PeerError::Remote(error));
        }
        self.write(&WireMessage::Result(ResultMsg::success(
            activation.id,
            ResultKind::Activate,
            serde_json::to_value(ActivateOutput {})?,
        )))
        .await?;
        let Peer { transport, state } = self;
        Ok(Peer {
            transport,
            state: WorkerReady {
                negotiated_features: state.negotiated_features,
                host_operations: state.host_operations,
            },
        })
    }
}

impl<T> Peer<T, Ready>
where
    T: FrameTransport + 'static,
{
    /// Split a ready peer into a cloneable call handle and its explicitly-owned I/O driver.
    pub fn into_runtime(self) -> (crate::PeerHandle, crate::PeerDriver<T>) {
        crate::peer_runtime::runtime_parts(self.transport, self.state, Vec::new())
    }
}

impl<T> Peer<T, WorkerReady>
where
    T: FrameTransport + 'static,
{
    /// Split a ready worker peer into a cloneable call handle and its explicitly-owned I/O driver.
    pub fn into_runtime(self) -> (crate::PeerHandle, crate::PeerDriver<T>) {
        let WorkerReady {
            negotiated_features,
            host_operations,
        } = self.state;
        crate::peer_runtime::runtime_parts(
            self.transport,
            Ready {
                negotiated_features,
            },
            host_operations,
        )
    }
}

impl<T, State> Peer<T, State>
where
    T: FrameTransport + 'static,
{
    async fn read(&self) -> Result<WireMessage, PeerError> {
        let frame = read_traced_frame(self.transport.as_ref()).await?;
        Ok(parse_wire_message(&frame)?)
    }

    async fn write(&self, message: &WireMessage) -> Result<(), PeerError> {
        let payload = encode_wire_message(message)?;
        write_traced_frame(self.transport.as_ref(), &payload).await?;
        Ok(())
    }
}

fn validate_host_initialization(initialization: &HostInitialization) -> Result<(), PeerError> {
    if initialization.request_id.is_empty() {
        return Err(PeerError::Protocol(
            "initialize request id must not be empty".into(),
        ));
    }
    if initialization.extension_id.is_empty() {
        return Err(PeerError::Protocol(
            "expected extension id must not be empty".into(),
        ));
    }
    validate_feature_declaration(
        &initialization.supported_features,
        &initialization.required_features,
    )?;
    validate_host_operations(&initialization.host_operations).map_err(PeerError::Remote)
}

fn validate_feature_declaration(
    supported: &BTreeSet<FeatureName>,
    required: &BTreeSet<FeatureName>,
) -> Result<(), PeerError> {
    if required.is_subset(supported) {
        Ok(())
    } else {
        Err(PeerError::Protocol(
            "required features must also be declared as supported".into(),
        ))
    }
}

fn validate_host_operations(operations: &[String]) -> Result<(), ErrorPayload> {
    let mut names = BTreeSet::new();
    for operation in operations {
        if operation.is_empty() {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidRequest,
                "host operation name must not be empty",
            ));
        }
        if !names.insert(operation.as_str()) {
            return Err(ErrorPayload::new(
                WireErrorCode::InvalidRequest,
                format!("duplicate host operation {operation}"),
            ));
        }
    }
    Ok(())
}

fn validate_initialize(
    initialize: &InitializeMsg,
    local_peer: &PeerInfo,
    local_supported: &BTreeSet<FeatureName>,
    local_required: &BTreeSet<FeatureName>,
) -> Result<BTreeSet<FeatureName>, ErrorPayload> {
    if initialize.id.is_empty() {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidRequest,
            "initialize request id must not be empty",
        ));
    }
    if initialize.protocol_version != S5R_VERSION {
        return Err(ErrorPayload::new(
            WireErrorCode::UnsupportedProtocolVersion,
            format!(
                "unsupported S5R version {}; expected {S5R_VERSION}",
                initialize.protocol_version
            ),
        ));
    }
    initialize.host.validate()?;
    if initialize.extension_id != local_peer.name {
        return Err(ErrorPayload::new(
            WireErrorCode::InvalidRequest,
            format!(
                "host expected extension {:?}, worker identity is {:?}",
                initialize.extension_id, local_peer.name
            ),
        ));
    }
    validate_host_operations(&initialize.host_operations)?;
    let negotiated = negotiate_features(
        local_supported,
        &initialize.supported_features,
        &initialize.required_features,
    )?;
    if !local_required.is_subset(&negotiated) {
        let missing = local_required
            .difference(&negotiated)
            .map(FeatureName::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        return Err(ErrorPayload::new(
            WireErrorCode::UnsupportedFeature,
            format!("host does not support required features: {missing}"),
        ));
    }
    Ok(negotiated)
}

fn validate_initialize_output(
    output: &InitializeOutput,
    expected_extension_id: &str,
    supported_features: &BTreeSet<FeatureName>,
    required_features: &BTreeSet<FeatureName>,
) -> Result<(), PeerError> {
    if output.protocol_version != S5R_VERSION {
        return Err(PeerError::Protocol(format!(
            "worker selected S5R {}; expected {S5R_VERSION}",
            output.protocol_version
        )));
    }
    output.worker.validate().map_err(PeerError::Remote)?;
    if output.worker.name != expected_extension_id {
        return Err(PeerError::Protocol(format!(
            "expected extension {expected_extension_id:?}, worker identified as {:?}",
            output.worker.name
        )));
    }
    let remote_supported: BTreeSet<_> = output.supported_features.iter().cloned().collect();
    let remote_required: BTreeSet<_> = output.required_features.iter().cloned().collect();
    if remote_supported.len() != output.supported_features.len()
        || remote_required.len() != output.required_features.len()
        || !remote_required.is_subset(&remote_supported)
    {
        return Err(PeerError::Protocol(
            "worker returned an invalid feature catalog".into(),
        ));
    }
    let negotiated: BTreeSet<_> = output.negotiated_features.iter().cloned().collect();
    if negotiated.len() != output.negotiated_features.len() {
        return Err(PeerError::Protocol(
            "worker returned duplicate negotiated features".into(),
        ));
    }
    let expected: BTreeSet<_> = supported_features
        .intersection(&remote_supported)
        .cloned()
        .collect();
    if negotiated != expected
        || !required_features.is_subset(&negotiated)
        || !remote_required.is_subset(&negotiated)
    {
        return Err(PeerError::Protocol(
            "worker returned an invalid negotiated feature set".into(),
        ));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum PeerError {
    #[error(transparent)]
    Frame(#[from] FrameError),
    #[error(transparent)]
    ProtocolCodec(#[from] ProtocolError),
    #[error("protocol error: {0}")]
    Protocol(String),
    #[error("expected {0}")]
    UnexpectedMessage(&'static str),
    #[error("remote peer error: {0}")]
    Remote(ErrorPayload),
}

impl From<serde_json::Error> for PeerError {
    fn from(error: serde_json::Error) -> Self {
        Self::Protocol(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use astrcode_extension_sdk::wire::frame::FrameTransport;
    use tokio::{
        io::{DuplexStream, ReadHalf, WriteHalf},
        sync::Mutex,
    };

    use super::*;

    #[derive(Clone)]
    struct DuplexTransport {
        reader: Arc<Mutex<ReadHalf<DuplexStream>>>,
        writer: Arc<Mutex<WriteHalf<DuplexStream>>>,
    }

    impl DuplexTransport {
        fn pair() -> (Self, Self) {
            let (left, right) = tokio::io::duplex(64 * 1024);
            let (left_reader, left_writer) = tokio::io::split(left);
            let (right_reader, right_writer) = tokio::io::split(right);
            (
                Self {
                    reader: Arc::new(Mutex::new(left_reader)),
                    writer: Arc::new(Mutex::new(left_writer)),
                },
                Self {
                    reader: Arc::new(Mutex::new(right_reader)),
                    writer: Arc::new(Mutex::new(right_writer)),
                },
            )
        }
    }

    #[async_trait::async_trait]
    impl FrameTransport for DuplexTransport {
        async fn read_frame(&self) -> Result<Vec<u8>, FrameError> {
            let mut header = Vec::new();
            let mut reader = self.reader.lock().await;
            use tokio::io::AsyncReadExt;
            loop {
                let byte = reader.read_u8().await?;
                if byte == b'\n' {
                    break;
                }
                header.push(byte);
            }
            let size = astrcode_extension_sdk::wire::frame::parse_frame_header(&header)?;
            let mut payload = vec![0; size];
            reader.read_exact(&mut payload).await?;
            Ok(payload)
        }

        async fn write_frame(&self, payload: &[u8]) -> Result<(), FrameError> {
            use tokio::io::AsyncWriteExt;
            let mut writer = self.writer.lock().await;
            writer
                .write_all(&astrcode_extension_sdk::wire::frame::frame_payload(
                    payload,
                )?)
                .await?;
            writer.flush().await?;
            Ok(())
        }
    }

    fn info(name: &str) -> PeerInfo {
        PeerInfo {
            name: name.into(),
            version: None,
        }
    }

    #[tokio::test]
    async fn initialization_and_activation_preserve_typed_role_declarations() {
        let (host_transport, worker_transport) = DuplexTransport::pair();
        let host = Peer::new(host_transport, info("host"));
        let worker = Peer::new(worker_transport, info("worker"));

        let mut host_initialization = HostInitialization::new("initialize-1", "worker");
        host_initialization.supported_features = BTreeSet::from([
            FeatureName::nested_invoke_v1(),
            FeatureName::model_stream_v1(),
        ]);
        host_initialization.required_features = BTreeSet::from([FeatureName::nested_invoke_v1()]);
        host_initialization.host_operations = vec!["astrcode.test".into()];
        let mut worker_initialization = WorkerInitialization::new(InitializeManifest::default());
        worker_initialization.supported_features = BTreeSet::from([
            FeatureName::nested_invoke_v1(),
            FeatureName::custom_event_v1(),
        ]);

        let (host, worker) = tokio::join!(
            host.initialize(host_initialization),
            worker.accept(worker_initialization)
        );
        let (host, worker_peer, manifest) = host.unwrap();
        let worker = worker.unwrap();
        let expected = BTreeSet::from([FeatureName::nested_invoke_v1()]);
        assert_eq!(host.negotiated_features(), &expected);
        assert_eq!(worker_peer.name, "worker");
        assert!(manifest.tools.is_empty());

        let expected_config = serde_json::json!({ "maxOutputTokens": 2048 });
        let observed_config = Arc::new(Mutex::new(None));
        let worker_config = Arc::clone(&observed_config);
        let (host, worker) = tokio::join!(
            host.activate("activate-1", expected_config.clone()),
            worker.accept_activation(move |config| async move {
                *worker_config.lock().await = Some(config);
                Ok(())
            })
        );
        let host = host.unwrap();
        let worker = worker.unwrap();
        assert_eq!(*observed_config.lock().await, Some(expected_config));
        let (host, _) = host.into_runtime();
        let (worker, _) = worker.into_runtime();
        assert!(!host.host_supports("astrcode.test"));
        assert!(worker.host_supports("astrcode.test"));
    }

    #[tokio::test]
    async fn activation_reports_worker_config_rejection_to_both_peers() {
        let (host_transport, worker_transport) = DuplexTransport::pair();
        let host = Peer::new(host_transport, info("host"));
        let worker = Peer::new(worker_transport, info("worker"));
        let (host, worker) = tokio::join!(
            host.initialize(HostInitialization::new("initialize-1", "worker")),
            worker.accept(WorkerInitialization::new(InitializeManifest::default()))
        );
        let (host, _, _) = host.unwrap();
        let worker = worker.unwrap();
        let (host_result, worker_result) = tokio::join!(
            host.activate("activate-1", serde_json::json!({ "invalid": true })),
            worker.accept_activation(|config| async move {
                assert_eq!(config, serde_json::json!({ "invalid": true }));
                Err(ErrorPayload::new(
                    WireErrorCode::InvalidRequest,
                    "invalid extension config",
                ))
            })
        );

        assert!(matches!(host_result, Err(PeerError::Remote(_))));
        assert!(matches!(worker_result, Err(PeerError::Remote(_))));
    }

    #[tokio::test]
    async fn worker_rejects_identity_mismatch_and_duplicate_host_operations() {
        for (extension_id, operations) in [
            ("different-worker", vec!["astrcode.test".into()]),
            (
                "worker",
                vec!["astrcode.test".into(), "astrcode.test".into()],
            ),
            ("worker", vec![String::new()]),
        ] {
            let (host_transport, worker_transport) = DuplexTransport::pair();
            let host = Peer::new(host_transport, info("host"));
            let worker = Peer::new(worker_transport, info("worker"));
            let mut host_initialization = HostInitialization::new("initialize-1", extension_id);
            host_initialization.host_operations = operations;

            let (host_result, worker_result) = tokio::join!(
                host.initialize(host_initialization),
                worker.accept(WorkerInitialization::new(InitializeManifest::default()))
            );
            assert!(host_result.is_err());
            assert!(worker_result.is_err());
        }
    }

    #[tokio::test]
    async fn worker_rejects_business_messages_before_activation() {
        let (host_transport, worker_transport) = DuplexTransport::pair();
        let raw_host = host_transport.clone();
        let host = Peer::new(host_transport, info("host"));
        let worker = Peer::new(worker_transport, info("worker"));
        let (host, worker) = tokio::join!(
            host.initialize(HostInitialization::new("initialize-1", "worker")),
            worker.accept(WorkerInitialization::new(InitializeManifest::default()))
        );
        let (_host, _worker_peer, _manifest) = host.unwrap();
        let worker = worker.unwrap();

        let message = WireMessage::Invoke(astrcode_extension_sdk::wire::protocol::InvokeMsg {
            id: "invoke-1".into(),
            operation: "astrcode.test".into(),
            input: serde_json::Value::Null,
            stream: false,
            parent_invoke_id: None,
        });
        raw_host
            .write_frame(&encode_wire_message(&message).unwrap())
            .await
            .unwrap();

        assert!(matches!(
            worker.accept_activation(|_| async { Ok(()) }).await,
            Err(PeerError::UnexpectedMessage("activate request"))
        ));
    }
}
