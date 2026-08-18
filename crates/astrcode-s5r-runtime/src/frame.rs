//! Traced framing helpers for the S5R subprocess runtime.
//!
//! Frame primitives (`FrameTransport`, `FrameError`, `StdioFrameTransport`)
//! stay in `astrcode_extension_sdk::wire::frame`; these runtime-side helpers
//! wrap read/write with the stable `s5r.frame` span so host and worker logs
//! record frame direction and byte counts.

use astrcode_extension_sdk::wire::frame::{FrameError, FrameTransport};
use tracing::Instrument;

pub async fn read_traced_frame<T>(transport: &T) -> Result<Vec<u8>, FrameError>
where
    T: FrameTransport + ?Sized,
{
    async {
        let frame = transport.read_frame().await?;
        tracing::Span::current().record("bytes", frame.len());
        Ok(frame)
    }
    .instrument(tracing::trace_span!(
        "s5r.frame",
        direction = "inbound",
        bytes = tracing::field::Empty,
    ))
    .await
}

pub async fn write_traced_frame<T>(transport: &T, payload: &[u8]) -> Result<(), FrameError>
where
    T: FrameTransport + ?Sized,
{
    transport
        .write_frame(payload)
        .instrument(tracing::trace_span!(
            "s5r.frame",
            direction = "outbound",
            bytes = payload.len(),
        ))
        .await
}
