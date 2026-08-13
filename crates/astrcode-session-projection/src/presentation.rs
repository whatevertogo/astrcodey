//! 不进入 provider context 的稳定展示事实。

use astrcode_core::event::{DurableEventPayload, StoredEvent};
use serde::{Deserialize, Serialize};

/// 不进入 provider 上下文、但需要稳定展示的 durable 记录。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum SessionArtifactView {
    Error {
        id: String,
        message: String,
        seq: u64,
    },
    SystemNote {
        id: String,
        text: String,
        seq: u64,
    },
}

impl SessionArtifactView {
    pub fn seq(&self) -> u64 {
        match self {
            Self::Error { seq, .. } | Self::SystemNote { seq, .. } => *seq,
        }
    }
}

/// 用于会话命名和稳定展示、但不进入 provider context 的状态。
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionPresentation {
    pub first_user_message: Option<String>,
    pub artifacts: Vec<SessionArtifactView>,
}

pub(crate) fn apply_event(event: &StoredEvent, presentation: &mut SessionPresentation) {
    match &event.payload {
        DurableEventPayload::UserMessage { text, .. } => {
            if presentation.first_user_message.is_none() {
                presentation.first_user_message = Some(text.clone());
            }
        },
        DurableEventPayload::TranscriptRewritten { source_seq, .. } => {
            presentation
                .artifacts
                .retain(|artifact| artifact.seq() > *source_seq);
        },
        DurableEventPayload::SessionForked {
            first_user_message, ..
        } => {
            presentation.first_user_message = first_user_message.clone();
        },
        DurableEventPayload::ErrorOccurred { message, .. } => {
            presentation.artifacts.push(SessionArtifactView::Error {
                id: event.id.to_string(),
                message: message.clone(),
                seq: event.seq,
            });
        },
        DurableEventPayload::RecapGenerated { text, .. } => {
            presentation
                .artifacts
                .push(SessionArtifactView::SystemNote {
                    id: event.id.to_string(),
                    text: text.clone(),
                    seq: event.seq,
                });
        },
        _ => {},
    }
}
