//! Projection 顺序、会话归属与 transcript rewrite 校验错误。

use astrcode_core::types::SessionId;
use thiserror::Error;

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum ProjectionError {
    #[error("projection batch cannot be empty")]
    EmptyBatch,
    #[error("session event sequence overflow")]
    SequenceOverflow,
    #[error("session {0} has no SessionStarted event")]
    MissingSessionStarted(SessionId),
    #[error("event belongs to session {actual}, expected {expected}")]
    SessionMismatch {
        expected: SessionId,
        actual: SessionId,
    },
    #[error("first event must have seq 0, got {0}")]
    InvalidFirstSequence(u64),
    #[error("first event must be SessionStarted")]
    InvalidFirstEvent,
    #[error("SessionStarted must be a session-level event")]
    SessionStartedHasTurn,
    #[error("duplicate SessionStarted at seq {0}")]
    DuplicateSessionStarted(u64),
    #[error("expected event seq {expected}, got {actual}")]
    NonContiguousSequence { expected: u64, actual: u64 },
    #[error("transcript rewrite source seq {source_seq} exceeds current seq {current_seq}")]
    InvalidTranscriptRewriteSource { source_seq: u64, current_seq: u64 },
    #[error(
        "transcript rewrite source fingerprint mismatch at seq {source_seq}: event expects \
         {expected}, current prefix hashes to {actual}"
    )]
    TranscriptRewriteSourceFingerprintMismatch {
        source_seq: u64,
        expected: String,
        actual: String,
    },
    #[error("transcript fingerprint could not be computed: {0}")]
    TranscriptFingerprintSerialization(String),
}
