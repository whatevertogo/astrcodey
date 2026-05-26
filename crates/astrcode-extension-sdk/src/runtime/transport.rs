//! stdio 长度前缀帧传输。

use std::{io, sync::Arc};

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, BufReader, BufWriter, Stdin, Stdout},
    process::{ChildStdin, ChildStdout},
    sync::Mutex as AsyncMutex,
};

const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

pub fn frame_payload(payload: &[u8]) -> Vec<u8> {
    let mut out = format!("{}\n", payload.len()).into_bytes();
    out.extend_from_slice(payload);
    out
}

pub fn parse_frame_header(header: &[u8]) -> Result<usize, String> {
    let header = std::str::from_utf8(header)
        .map_err(|e| format!("invalid frame header utf8: {e}"))?
        .trim();
    if header.is_empty() {
        return Err("empty frame header".into());
    }
    let size: usize = header
        .parse()
        .map_err(|_| format!("invalid frame header: {header:?}"))?;
    if size > MAX_FRAME_BYTES {
        return Err(format!("frame size {size} exceeds max {MAX_FRAME_BYTES}"));
    }
    Ok(size)
}

async fn read_frame_from<R: AsyncReadExt + Unpin>(reader: &mut R) -> Result<Vec<u8>, io::Error> {
    let mut header = Vec::new();
    loop {
        let byte = reader.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        header.push(byte);
        if header.len() > 32 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "frame header too long",
            ));
        }
    }
    let size = parse_frame_header(&header).map_err(io::Error::other)?;
    let mut payload = vec![0u8; size];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

async fn write_frame_to<W: AsyncWriteExt + Unpin>(
    writer: &mut W,
    payload: &[u8],
) -> Result<(), io::Error> {
    let frame = frame_payload(payload);
    writer.write_all(&frame).await?;
    writer.flush().await?;
    Ok(())
}

/// 读写长度前缀帧（宿主侧：子进程 stdio）。
pub struct StdioFrameTransport {
    reader: Arc<AsyncMutex<BufReader<ChildStdout>>>,
    writer: Arc<AsyncMutex<BufWriter<ChildStdin>>>,
}

impl StdioFrameTransport {
    pub fn new(stdin: ChildStdin, stdout: ChildStdout) -> Self {
        Self {
            reader: Arc::new(AsyncMutex::new(BufReader::new(stdout))),
            writer: Arc::new(AsyncMutex::new(BufWriter::new(stdin))),
        }
    }
}

/// 当前进程 stdio（Worker 侧）。
pub struct ProcessStdioTransport {
    reader: Arc<AsyncMutex<BufReader<Stdin>>>,
    writer: Arc<AsyncMutex<BufWriter<Stdout>>>,
}

impl ProcessStdioTransport {
    pub fn new() -> Self {
        Self {
            reader: Arc::new(AsyncMutex::new(BufReader::new(tokio::io::stdin()))),
            writer: Arc::new(AsyncMutex::new(BufWriter::new(tokio::io::stdout()))),
        }
    }
}

impl Default for ProcessStdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

/// 抽象帧传输（测试与 Peer 解耦）。
#[async_trait::async_trait]
pub trait FrameTransport: Send + Sync {
    async fn read_frame(&self) -> Result<Vec<u8>, io::Error>;
    async fn write_frame(&self, payload: &[u8]) -> Result<(), io::Error>;
}

#[async_trait::async_trait]
impl FrameTransport for StdioFrameTransport {
    async fn read_frame(&self) -> Result<Vec<u8>, io::Error> {
        let mut reader = self.reader.lock().await;
        read_frame_from(&mut *reader).await
    }

    async fn write_frame(&self, payload: &[u8]) -> Result<(), io::Error> {
        let mut writer = self.writer.lock().await;
        write_frame_to(&mut *writer, payload).await
    }
}

#[async_trait::async_trait]
impl FrameTransport for ProcessStdioTransport {
    async fn read_frame(&self) -> Result<Vec<u8>, io::Error> {
        let mut reader = self.reader.lock().await;
        read_frame_from(&mut *reader).await
    }

    async fn write_frame(&self, payload: &[u8]) -> Result<(), io::Error> {
        let mut writer = self.writer.lock().await;
        write_frame_to(&mut *writer, payload).await
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::io::{DuplexStream, ReadHalf, WriteHalf};

    use super::*;
    use crate::{
        runtime::Peer,
        s5r::{InitializeOutput, PeerInfo, S5R_VERSION},
    };

    struct DuplexFrameTransport {
        reader: Arc<AsyncMutex<ReadHalf<DuplexStream>>>,
        writer: Arc<AsyncMutex<WriteHalf<DuplexStream>>>,
    }

    impl DuplexFrameTransport {
        fn pair() -> (Self, Self) {
            let (a, b) = tokio::io::duplex(65536);
            let (a_read, a_write) = tokio::io::split(a);
            let (b_read, b_write) = tokio::io::split(b);
            (
                Self {
                    reader: Arc::new(AsyncMutex::new(a_read)),
                    writer: Arc::new(AsyncMutex::new(a_write)),
                },
                Self {
                    reader: Arc::new(AsyncMutex::new(b_read)),
                    writer: Arc::new(AsyncMutex::new(b_write)),
                },
            )
        }
    }

    #[async_trait::async_trait]
    impl FrameTransport for DuplexFrameTransport {
        async fn read_frame(&self) -> Result<Vec<u8>, io::Error> {
            let mut reader = self.reader.lock().await;
            read_frame_from(&mut *reader).await
        }

        async fn write_frame(&self, payload: &[u8]) -> Result<(), io::Error> {
            let mut writer = self.writer.lock().await;
            write_frame_to(&mut *writer, payload).await
        }
    }

    #[tokio::test]
    async fn duplex_frame_transport_write_read() {
        let (host_t, worker_t) = DuplexFrameTransport::pair();
        let payload = br#"{"type":"initialize","id":"1"}"#;
        let worker_t = Arc::new(worker_t);
        let worker_t2 = Arc::clone(&worker_t);
        tokio::spawn(async move {
            worker_t2.write_frame(payload).await.unwrap();
        });
        let frame = host_t.read_frame().await.unwrap();
        assert_eq!(frame, payload);
    }

    #[tokio::test]
    async fn peer_initialize_handshake_duplex() {
        let (host_transport, worker_transport) = DuplexFrameTransport::pair();

        let host = Peer::new(
            host_transport,
            PeerInfo {
                name: "astrcode-host".into(),
                role: "core".into(),
                version: Some("astrcode".into()),
            },
        );
        let worker = Peer::new(
            worker_transport,
            PeerInfo {
                name: "s5r-guest-demo".into(),
                role: "plugin".into(),
                version: Some("astrcode".into()),
            },
        );

        host.set_initialize_handler(Arc::new(|_init| {
            Box::pin(async move {
                Ok(InitializeOutput {
                    peer: PeerInfo {
                        name: "astrcode-host".into(),
                        role: "core".into(),
                        version: Some("astrcode".into()),
                    },
                    protocol_version: Some(S5R_VERSION.into()),
                    capabilities: Vec::new(),
                    metadata: serde_json::json!({ "wire_codec": "json" }),
                })
            })
        }));

        host.start().await.unwrap();
        worker.start().await.unwrap();

        let metadata = serde_json::json!({
            "extension_id": "s5r-guest-demo",
            "version": "0.1.0",
            "protocol": { "s5r": "1.0" },
        });
        let worker = Arc::clone(&worker);
        let init_task = tokio::spawn(async move { worker.initialize(Vec::new(), metadata).await });
        host.wait_remote_initialized(std::time::Duration::from_secs(5))
            .await
            .unwrap();
        init_task.await.unwrap().unwrap();
    }

    #[test]
    fn frame_roundtrip_format() {
        let payload = br#"{"type":"invoke"}"#;
        let framed = frame_payload(payload);
        let nl = framed.iter().position(|b| *b == b'\n').unwrap();
        let size = parse_frame_header(&framed[..=nl]).unwrap();
        assert_eq!(size, payload.len());
        assert_eq!(&framed[nl + 1..], payload);
    }
}
