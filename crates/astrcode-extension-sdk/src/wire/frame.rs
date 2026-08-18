use std::{io, sync::Arc};

use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter, Stdin, Stdout},
    process::{ChildStdin, ChildStdout},
    sync::Mutex,
};

pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_FRAME_HEADER_BYTES: usize = 32;

pub fn frame_payload(payload: &[u8]) -> Result<Vec<u8>, FrameError> {
    if payload.len() > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            size: payload.len(),
            max: MAX_FRAME_BYTES,
        });
    }
    let mut frame = format!("{}\n", payload.len()).into_bytes();
    frame.extend_from_slice(payload);
    Ok(frame)
}

pub fn parse_frame_header(header: &[u8]) -> Result<usize, FrameError> {
    let header = header.strip_suffix(b"\n").unwrap_or(header);
    if header.is_empty() {
        return Err(FrameError::InvalidHeader("empty frame header".into()));
    }
    if header.len() > MAX_FRAME_HEADER_BYTES {
        return Err(FrameError::HeaderTooLong);
    }
    if !header.iter().all(u8::is_ascii_digit) {
        return Err(FrameError::InvalidHeader(
            "frame header must contain decimal digits only".into(),
        ));
    }
    let header = std::str::from_utf8(header)
        .map_err(|error| FrameError::InvalidHeader(error.to_string()))?;
    let size = header
        .parse::<usize>()
        .map_err(|error| FrameError::InvalidHeader(error.to_string()))?;
    if size > MAX_FRAME_BYTES {
        return Err(FrameError::TooLarge {
            size,
            max: MAX_FRAME_BYTES,
        });
    }
    Ok(size)
}

async fn read_frame_from<R>(reader: &mut R) -> Result<Vec<u8>, FrameError>
where
    R: AsyncRead + Unpin + Send,
{
    let mut header = Vec::new();
    loop {
        let byte = reader.read_u8().await?;
        if byte == b'\n' {
            break;
        }
        if header.len() == MAX_FRAME_HEADER_BYTES {
            return Err(FrameError::HeaderTooLong);
        }
        header.push(byte);
    }
    let size = parse_frame_header(&header)?;
    let mut payload = vec![0; size];
    reader.read_exact(&mut payload).await?;
    Ok(payload)
}

async fn write_frame_to<W>(writer: &mut W, payload: &[u8]) -> Result<(), FrameError>
where
    W: AsyncWrite + Unpin + Send,
{
    writer.write_all(&frame_payload(payload)?).await?;
    writer.flush().await?;
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum FrameError {
    #[error("frame I/O failed: {0}")]
    Io(#[from] io::Error),
    #[error("invalid frame header: {0}")]
    InvalidHeader(String),
    #[error("frame header exceeds {MAX_FRAME_HEADER_BYTES} bytes")]
    HeaderTooLong,
    #[error("frame size {size} exceeds max {max}")]
    TooLarge { size: usize, max: usize },
}

#[async_trait::async_trait]
pub trait FrameTransport: Send + Sync {
    async fn read_frame(&self) -> Result<Vec<u8>, FrameError>;
    async fn write_frame(&self, payload: &[u8]) -> Result<(), FrameError>;
}

pub struct StdioFrameTransport {
    reader: Arc<Mutex<BufReader<ChildStdout>>>,
    writer: Arc<Mutex<BufWriter<ChildStdin>>>,
}

impl StdioFrameTransport {
    pub fn new(stdin: ChildStdin, stdout: ChildStdout) -> Self {
        Self {
            reader: Arc::new(Mutex::new(BufReader::new(stdout))),
            writer: Arc::new(Mutex::new(BufWriter::new(stdin))),
        }
    }
}

#[async_trait::async_trait]
impl FrameTransport for StdioFrameTransport {
    async fn read_frame(&self) -> Result<Vec<u8>, FrameError> {
        read_frame_from(&mut *self.reader.lock().await).await
    }

    async fn write_frame(&self, payload: &[u8]) -> Result<(), FrameError> {
        write_frame_to(&mut *self.writer.lock().await, payload).await
    }
}

pub struct ProcessStdioTransport {
    reader: Arc<Mutex<BufReader<Stdin>>>,
    writer: Arc<Mutex<BufWriter<Stdout>>>,
}

impl ProcessStdioTransport {
    pub fn new() -> Self {
        Self {
            reader: Arc::new(Mutex::new(BufReader::new(tokio::io::stdin()))),
            writer: Arc::new(Mutex::new(BufWriter::new(tokio::io::stdout()))),
        }
    }
}

impl Default for ProcessStdioTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl FrameTransport for ProcessStdioTransport {
    async fn read_frame(&self) -> Result<Vec<u8>, FrameError> {
        read_frame_from(&mut *self.reader.lock().await).await
    }

    async fn write_frame(&self, payload: &[u8]) -> Result<(), FrameError> {
        write_frame_to(&mut *self.writer.lock().await, payload).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_header_is_strict_and_bounded() {
        let cases: &[(&[u8], Result<usize, &str>)] = &[
            (b"0", Ok(0)),
            (b"12\n", Ok(12)),
            (b"", Err("empty")),
            (b" 12", Err("decimal")),
            (b"+12", Err("decimal")),
        ];
        for (header, expected) in cases {
            match expected {
                Ok(size) => assert_eq!(parse_frame_header(header).unwrap(), *size),
                Err(fragment) => assert!(
                    parse_frame_header(header)
                        .unwrap_err()
                        .to_string()
                        .contains(fragment)
                ),
            }
        }
        assert!(matches!(
            parse_frame_header((MAX_FRAME_BYTES + 1).to_string().as_bytes()),
            Err(FrameError::TooLarge { .. })
        ));
    }
}
