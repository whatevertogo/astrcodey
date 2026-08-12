use std::{collections::BTreeSet, marker::PhantomData, sync::Arc};

use serde_json::Value;

use crate::{
    WireErrorCode,
    frame::{FrameError, FrameTransport, read_traced_frame, write_traced_frame},
    protocol::{
        CapabilityDescriptor, ErrorPayload, FeatureName, HandlerDescriptor, InitializeMsg,
        InitializeOutput, PeerInfo, ProtocolError, ResultKind, ResultMsg, S5R_VERSION, WireMessage,
        encode_wire_message, negotiate_features, parse_wire_message,
    },
};

pub struct Uninitialized;

pub struct Ready {
    pub(crate) remote_peer: PeerInfo,
    pub(crate) negotiated_features: BTreeSet<FeatureName>,
    pub(crate) remote_handlers: Vec<HandlerDescriptor>,
    pub(crate) remote_capabilities: Vec<CapabilityDescriptor>,
    pub(crate) remote_metadata: Value,
}

pub struct PeerHandshake {
    pub request_id: String,
    pub supported_features: BTreeSet<FeatureName>,
    pub required_features: BTreeSet<FeatureName>,
    pub handlers: Vec<HandlerDescriptor>,
    pub capabilities: Vec<CapabilityDescriptor>,
    pub metadata: Value,
}

impl PeerHandshake {
    pub fn new(request_id: impl Into<String>) -> Self {
        Self {
            request_id: request_id.into(),
            supported_features: BTreeSet::new(),
            required_features: BTreeSet::new(),
            handlers: Vec::new(),
            capabilities: Vec::new(),
            metadata: Value::Null,
        }
    }
}

/// An S5R peer whose callable surface is determined by its handshake state.
///
/// ```compile_fail
/// use astrcode_extension_contract::{Peer, PeerInfo, ProcessStdioTransport};
///
/// let peer = Peer::new(
///     ProcessStdioTransport::new(),
///     PeerInfo { name: "worker".into(), role: "plugin".into(), version: None },
/// );
/// peer.negotiated_features();
/// ```
pub struct Peer<T, State = Uninitialized> {
    transport: Arc<T>,
    local_peer: PeerInfo,
    state: State,
    marker: PhantomData<fn() -> State>,
}

impl<T> Peer<T, Uninitialized>
where
    T: FrameTransport + 'static,
{
    pub fn new(transport: T, local_peer: PeerInfo) -> Self {
        Self {
            transport: Arc::new(transport),
            local_peer,
            state: Uninitialized,
            marker: PhantomData,
        }
    }

    pub async fn initialize(self, handshake: PeerHandshake) -> Result<Peer<T, Ready>, PeerError> {
        self.local_peer
            .validate()
            .map_err(|error| PeerError::Protocol(error.to_string()))?;
        validate_handshake(&handshake)?;
        let initialize = InitializeMsg {
            id: handshake.request_id.clone(),
            protocol_version: S5R_VERSION.into(),
            peer: self.local_peer.clone(),
            supported_features: handshake.supported_features.iter().cloned().collect(),
            required_features: handshake.required_features.iter().cloned().collect(),
            handlers: handshake.handlers,
            provided_capabilities: handshake.capabilities,
            metadata: handshake.metadata,
        };
        self.write(&WireMessage::Initialize(initialize)).await?;

        let WireMessage::Result(result) = self.read().await? else {
            return Err(PeerError::UnexpectedMessage("initialize result"));
        };
        if result.id() != handshake.request_id || result.kind() != ResultKind::Initialize {
            return Err(PeerError::UnexpectedMessage("matching initialize result"));
        }
        let output = match result {
            ResultMsg::Success { output, .. } => output,
            ResultMsg::Failure { error, .. } => {
                return Err(PeerError::Remote(error));
            },
        };
        let output: InitializeOutput = serde_json::from_value(output)
            .map_err(|error| PeerError::Protocol(error.to_string()))?;
        validate_initialize_output(
            &output,
            &handshake.supported_features,
            &handshake.required_features,
        )?;
        Ok(self.ready(
            output.peer,
            output.negotiated_features.into_iter().collect(),
            output.handlers,
            output.capabilities,
            output.metadata,
        ))
    }

    pub async fn accept(
        self,
        local_supported: BTreeSet<FeatureName>,
        local_required: BTreeSet<FeatureName>,
        handlers: Vec<HandlerDescriptor>,
        capabilities: Vec<CapabilityDescriptor>,
        metadata: Value,
    ) -> Result<Peer<T, Ready>, PeerError> {
        self.local_peer
            .validate()
            .map_err(|error| PeerError::Protocol(error.to_string()))?;
        if !local_required.is_subset(&local_supported) {
            return Err(PeerError::Protocol(
                "required features must also be declared as supported".into(),
            ));
        }
        let WireMessage::Initialize(initialize) = self.read().await? else {
            return Err(PeerError::UnexpectedMessage("initialize request"));
        };
        let result = validate_initialize(&initialize, &local_supported, &local_required);
        match result {
            Ok(negotiated) => {
                let output = InitializeOutput {
                    peer: self.local_peer.clone(),
                    protocol_version: S5R_VERSION.into(),
                    supported_features: local_supported.iter().cloned().collect(),
                    required_features: local_required.iter().cloned().collect(),
                    negotiated_features: negotiated.iter().cloned().collect(),
                    handlers,
                    capabilities,
                    metadata,
                };
                self.write(&WireMessage::Result(ResultMsg::success(
                    initialize.id,
                    ResultKind::Initialize,
                    serde_json::to_value(output)?,
                )))
                .await?;
                Ok(self.ready(
                    initialize.peer,
                    negotiated,
                    initialize.handlers,
                    initialize.provided_capabilities,
                    initialize.metadata,
                ))
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

    fn ready(
        self,
        remote_peer: PeerInfo,
        negotiated_features: BTreeSet<FeatureName>,
        remote_handlers: Vec<HandlerDescriptor>,
        remote_capabilities: Vec<CapabilityDescriptor>,
        remote_metadata: Value,
    ) -> Peer<T, Ready> {
        Peer {
            transport: self.transport,
            local_peer: self.local_peer,
            state: Ready {
                remote_peer,
                negotiated_features,
                remote_handlers,
                remote_capabilities,
                remote_metadata,
            },
            marker: PhantomData,
        }
    }

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

impl<T> Peer<T, Ready>
where
    T: FrameTransport + 'static,
{
    pub fn local_peer(&self) -> &PeerInfo {
        &self.local_peer
    }

    pub fn remote_peer(&self) -> &PeerInfo {
        &self.state.remote_peer
    }

    pub fn negotiated_features(&self) -> &BTreeSet<FeatureName> {
        &self.state.negotiated_features
    }

    pub fn supports(&self, feature: &FeatureName) -> bool {
        self.state.negotiated_features.contains(feature)
    }

    pub fn remote_handlers(&self) -> &[HandlerDescriptor] {
        &self.state.remote_handlers
    }

    pub fn remote_capabilities(&self) -> &[CapabilityDescriptor] {
        &self.state.remote_capabilities
    }

    pub fn remote_metadata(&self) -> &Value {
        &self.state.remote_metadata
    }

    /// Split a ready peer into a cloneable call handle and its explicitly-owned I/O driver.
    ///
    /// The caller must keep [`crate::PeerDriver::run`] alive for as long as calls are admitted.
    /// All negotiated remote facts (peer, handlers, capabilities, metadata) remain readable on
    /// the handle after the split.
    pub fn into_runtime(self) -> (crate::PeerHandle<T>, crate::PeerDriver<T>) {
        crate::peer_runtime::runtime_parts(self.transport, self.state)
    }
}

fn validate_handshake(handshake: &PeerHandshake) -> Result<(), PeerError> {
    if handshake.request_id.is_empty() {
        return Err(PeerError::Protocol(
            "initialize request id must not be empty".into(),
        ));
    }
    if !handshake
        .required_features
        .is_subset(&handshake.supported_features)
    {
        return Err(PeerError::Protocol(
            "required features must also be declared as supported".into(),
        ));
    }
    Ok(())
}

fn validate_initialize(
    initialize: &InitializeMsg,
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
    initialize.peer.validate()?;
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
            format!("remote peer does not support required features: {missing}"),
        ));
    }
    Ok(negotiated)
}

fn validate_initialize_output(
    output: &InitializeOutput,
    supported_features: &BTreeSet<FeatureName>,
    required_features: &BTreeSet<FeatureName>,
) -> Result<(), PeerError> {
    if output.protocol_version != S5R_VERSION {
        return Err(PeerError::Protocol(format!(
            "remote selected S5R {}; expected {S5R_VERSION}",
            output.protocol_version
        )));
    }
    output.peer.validate().map_err(PeerError::Remote)?;
    let remote_supported: BTreeSet<_> = output.supported_features.iter().cloned().collect();
    let remote_required: BTreeSet<_> = output.required_features.iter().cloned().collect();
    if remote_supported.len() != output.supported_features.len()
        || remote_required.len() != output.required_features.len()
        || !remote_required.is_subset(&remote_supported)
    {
        return Err(PeerError::Protocol(
            "remote returned an invalid feature catalog".into(),
        ));
    }
    let negotiated: BTreeSet<_> = output.negotiated_features.iter().cloned().collect();
    if negotiated.len() != output.negotiated_features.len() {
        return Err(PeerError::Protocol(
            "remote returned duplicate negotiated features".into(),
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
            "remote returned an invalid negotiated feature set".into(),
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

    use tokio::{
        io::{DuplexStream, ReadHalf, WriteHalf},
        sync::Mutex,
    };

    use super::*;
    use crate::{
        frame::{FrameError, FrameTransport},
        protocol::{FeatureName, PeerInfo},
    };

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
            let size = crate::frame::parse_frame_header(&header)?;
            let mut payload = vec![0; size];
            reader.read_exact(&mut payload).await?;
            Ok(payload)
        }

        async fn write_frame(&self, payload: &[u8]) -> Result<(), FrameError> {
            use tokio::io::AsyncWriteExt;
            let mut writer = self.writer.lock().await;
            writer
                .write_all(&crate::frame::frame_payload(payload)?)
                .await?;
            writer.flush().await?;
            Ok(())
        }
    }

    fn info(name: &str, role: &str) -> PeerInfo {
        PeerInfo {
            name: name.into(),
            role: role.into(),
            version: None,
        }
    }

    #[tokio::test]
    async fn handshake_transitions_both_peers_to_the_same_negotiated_features() {
        let (host_transport, worker_transport) = DuplexTransport::pair();
        let host = Peer::new(host_transport, info("host", "host"));
        let worker = Peer::new(worker_transport, info("worker", "plugin"));

        let mut handshake = PeerHandshake::new("initialize-1");
        handshake.supported_features = BTreeSet::from([
            FeatureName::nested_invoke_v1(),
            FeatureName::model_stream_v1(),
        ]);
        handshake.required_features = BTreeSet::from([FeatureName::nested_invoke_v1()]);
        let worker_features = BTreeSet::from([
            FeatureName::nested_invoke_v1(),
            FeatureName::custom_event_v1(),
        ]);

        let (host, worker) = tokio::join!(
            host.initialize(handshake),
            worker.accept(
                worker_features,
                BTreeSet::new(),
                Vec::new(),
                Vec::new(),
                Value::Null,
            )
        );
        let host = host.unwrap();
        let worker = worker.unwrap();
        let expected = BTreeSet::from([FeatureName::nested_invoke_v1()]);
        assert_eq!(host.negotiated_features(), &expected);
        assert_eq!(worker.negotiated_features(), &expected);
        assert_eq!(host.remote_peer().name, "worker");
        assert_eq!(worker.remote_peer().name, "host");
    }
}
