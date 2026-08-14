use astrcode_core::llm::LlmMessage;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompactSnapshotInput {
    pub trigger: String,
    pub model_id: String,
    pub working_dir: String,
    pub system_prompt: Option<String>,
    pub provider_messages: Vec<LlmMessage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResultArtifactInput {
    pub call_id: String,
    pub tool_name: String,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ToolResultArtifactRef {
    pub bytes: usize,
    pub artifact_id: String,
}
