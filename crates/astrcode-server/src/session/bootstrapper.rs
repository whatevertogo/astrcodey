//! Session 执行前准备：工具快照、系统提示词和进程内工具状态。

use std::{collections::HashMap, sync::Arc};

use astrcode_context::prompt_engine::{PromptEngine, PromptFiles, load_system_prompt_files};
use astrcode_core::{
    config::ModelSelection,
    event::EventPayload,
    extension::{ExtensionError, PromptBuildContext},
    prompt::{ExtensionPromptBlock, ExtensionSection, PromptProvider, SystemPromptInput},
    tool::{FileObservationStore, ToolDefinition, ToolPromptMetadata},
    types::SessionId,
};
use astrcode_extensions::runner::ExtensionRunner;
use astrcode_session::SessionRuntimeRegistry;
use astrcode_support::{hash::hex_fingerprint, shell::resolve_shell};
use astrcode_tools::registry::{ToolRegistry, builtin_tools};
use parking_lot::Mutex;

use crate::{config_manager::ConfigManager, session::SessionDirectoryError};

struct SystemPromptSnapshotInput<'a> {
    extension_runner: &'a ExtensionRunner,
    session_id: &'a str,
    working_dir: &'a str,
    model_id: &'a str,
    tools: &'a [ToolDefinition],
    extra_system_prompt: Option<&'a str>,
    tool_prompt_metadata: HashMap<String, ToolPromptMetadata>,
    prompt_files: PromptFiles,
}

/// 组装 turn 前依赖，避免 router / actor 自己知道准备细节。
pub struct SessionBootstrapper {
    config: Arc<ConfigManager>,
    extension_runner: Arc<ExtensionRunner>,
    runtime_registry: Arc<SessionRuntimeRegistry>,
    tool_registries: Mutex<HashMap<SessionId, Arc<ToolRegistry>>>,
}

impl SessionBootstrapper {
    pub fn new(
        config: Arc<ConfigManager>,
        extension_runner: Arc<ExtensionRunner>,
        runtime_registry: Arc<SessionRuntimeRegistry>,
    ) -> Self {
        Self {
            config,
            extension_runner,
            runtime_registry,
            tool_registries: Mutex::new(HashMap::new()),
        }
    }

    pub async fn ensure_tool_registry(
        &self,
        session_id: &SessionId,
        working_dir: &str,
    ) -> Arc<ToolRegistry> {
        if let Some(registry) = self.tool_registries.lock().get(session_id).cloned() {
            return registry;
        }
        self.refresh_tool_registry(session_id, working_dir).await
    }

    pub async fn refresh_tool_registry(
        &self,
        session_id: &SessionId,
        working_dir: &str,
    ) -> Arc<ToolRegistry> {
        let timeout = self.config.read_effective().llm.read_timeout_secs;
        let registry =
            build_tool_registry_snapshot(&self.extension_runner, working_dir, timeout).await;
        self.tool_registries
            .lock()
            .insert(session_id.clone(), Arc::clone(&registry));
        registry
    }

    pub fn remove_session(&self, session_id: &SessionId) {
        self.tool_registries.lock().remove(session_id);
    }

    pub async fn initialize_system_prompt(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        extra_system_prompt: Option<&str>,
    ) -> Result<(Arc<ToolRegistry>, EventPayload), SessionDirectoryError> {
        let registry_fut = self.refresh_tool_registry(session_id, working_dir);
        let prompt_files_fut = load_system_prompt_files(working_dir);
        let (tool_registry, prompt_files) = tokio::join!(registry_fut, prompt_files_fut);
        let payload = self
            .configure_system_prompt_with_files(
                session_id,
                working_dir,
                &tool_registry,
                extra_system_prompt,
                prompt_files,
            )
            .await?;
        Ok((tool_registry, payload))
    }

    pub async fn configure_system_prompt(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
    ) -> Result<EventPayload, SessionDirectoryError> {
        let prompt_files = load_system_prompt_files(working_dir).await;
        self.configure_system_prompt_with_files(
            session_id,
            working_dir,
            tool_registry,
            extra_system_prompt,
            prompt_files,
        )
        .await
    }

    pub async fn build_system_prompt_snapshot(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
    ) -> Result<(String, String), SessionDirectoryError> {
        let prompt_files = load_system_prompt_files(working_dir).await;
        self.build_system_prompt_snapshot_with_files(
            session_id,
            working_dir,
            model_id,
            tool_registry,
            extra_system_prompt,
            prompt_files,
        )
        .await
    }

    pub fn file_observation_store(&self, session_id: &SessionId) -> Arc<dyn FileObservationStore> {
        self.runtime_registry
            .get_or_create(session_id)
            .file_observation_store()
    }

    async fn configure_system_prompt_with_files(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
        prompt_files: PromptFiles,
    ) -> Result<EventPayload, SessionDirectoryError> {
        let model_id = self.config.read_effective().llm.model_id.clone();
        let (system_prompt, fingerprint) = self
            .build_system_prompt_snapshot_with_files(
                session_id,
                working_dir,
                &model_id,
                tool_registry,
                extra_system_prompt,
                prompt_files,
            )
            .await?;
        Ok(EventPayload::SystemPromptConfigured {
            text: system_prompt,
            fingerprint,
        })
    }

    async fn build_system_prompt_snapshot_with_files(
        &self,
        session_id: &SessionId,
        working_dir: &str,
        model_id: &str,
        tool_registry: &ToolRegistry,
        extra_system_prompt: Option<&str>,
        prompt_files: PromptFiles,
    ) -> Result<(String, String), SessionDirectoryError> {
        let tools_with_meta = tool_registry.list_definitions_with_prompt_metadata();
        let tools: Vec<_> = tools_with_meta.iter().map(|(def, _)| def.clone()).collect();
        let tool_prompt_metadata = tools_with_meta
            .into_iter()
            .filter_map(|(def, meta)| meta.map(|m| (def.name, m)))
            .collect();
        build_system_prompt_snapshot_with_files(SystemPromptSnapshotInput {
            extension_runner: &self.extension_runner,
            session_id: session_id.as_str(),
            working_dir,
            model_id,
            tools: &tools,
            extra_system_prompt,
            tool_prompt_metadata,
            prompt_files,
        })
        .await
        .map_err(SessionDirectoryError::from)
    }
}

/// 构建一个工作目录绑定的工具表快照。
async fn build_tool_registry_snapshot(
    extension_runner: &ExtensionRunner,
    working_dir: &str,
    timeout_secs: u64,
) -> Arc<ToolRegistry> {
    let mut tool_registry = ToolRegistry::new();
    for tool in builtin_tools(std::path::PathBuf::from(working_dir), timeout_secs) {
        tool_registry.register(tool);
    }
    for tool in extension_runner
        .collect_tool_adapters_typed(working_dir)
        .await
        .into_iter()
        .rev()
    {
        tool_registry.register(tool);
    }
    Arc::new(tool_registry)
}

async fn build_system_prompt_snapshot_with_files(
    input: SystemPromptSnapshotInput<'_>,
) -> Result<(String, String), ExtensionError> {
    let SystemPromptSnapshotInput {
        extension_runner,
        session_id,
        working_dir,
        model_id,
        tools,
        extra_system_prompt,
        tool_prompt_metadata,
        prompt_files,
    } = input;
    let prompt_ctx = PromptBuildContext {
        session_id: session_id.to_string(),
        working_dir: working_dir.to_string(),
        model: ModelSelection::simple(model_id),
        tools: tools.to_vec(),
    };
    let contributions = extension_runner
        .collect_prompt_contributions_typed(prompt_ctx)
        .await?;
    let mut extension_blocks = Vec::new();
    for content in contributions.system_prompts {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::PlatformInstructions,
            content,
        });
    }
    for content in contributions.additional_instructions {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::AdditionalInstructions,
            content,
        });
    }
    for content in contributions.skills {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::Skills,
            content,
        });
    }
    for content in contributions.agents {
        extension_blocks.push(ExtensionPromptBlock {
            section: ExtensionSection::Agents,
            content,
        });
    }
    let extra_instructions = extra_system_prompt.and_then(|s| {
        let trimmed = s.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_string())
    });
    let mut merged_metadata = tool_prompt_metadata;
    merged_metadata.extend(extension_runner.collect_tool_prompt_metadata_typed().await);
    let input = SystemPromptInput {
        working_dir: working_dir.to_string(),
        os: std::env::consts::OS.into(),
        shell: resolve_shell().name,
        date: chrono::Utc::now().format("%Y-%m-%d").to_string(),
        identity: prompt_files.identity,
        user_rules: prompt_files.user_rules,
        project_rules: prompt_files.project_rules,
        tools: tools.to_vec(),
        tool_prompt_metadata: merged_metadata,
        extension_blocks,
        extra_instructions,
    };
    let system_prompt = PromptEngine::new()
        .assemble(input)
        .await
        .system_prompt
        .unwrap_or_default();
    let fingerprint = hex_fingerprint(system_prompt.as_bytes());
    Ok((system_prompt, fingerprint))
}

#[cfg(test)]
mod tests {
    use std::{sync::Arc, time::Duration};

    use astrcode_core::{
        extension::{Extension, Registrar, ToolHandler},
        tool::{ExecutionMode, ToolDefinition, ToolOrigin, ToolResult},
    };

    use super::*;

    struct StaticToolExtension {
        id: &'static str,
        tool_name: &'static str,
        description: &'static str,
    }

    #[async_trait::async_trait]
    impl Extension for StaticToolExtension {
        fn id(&self) -> &str {
            self.id
        }

        fn register(&self, reg: &mut Registrar) {
            reg.tool(
                ToolDefinition {
                    name: self.tool_name.into(),
                    description: self.description.into(),
                    parameters: serde_json::json!({"type": "object"}),
                    origin: ToolOrigin::Extension,
                    execution_mode: ExecutionMode::Sequential,
                },
                Arc::new(StaticToolHandler),
            );
        }
    }

    struct StaticToolHandler;

    #[async_trait::async_trait]
    impl ToolHandler for StaticToolHandler {
        async fn execute(
            &self,
            tool_name: &str,
            _arguments: serde_json::Value,
            _working_dir: &str,
            _ctx: &astrcode_core::tool::ToolExecutionContext,
        ) -> Result<ToolResult, astrcode_core::extension::ExtensionError> {
            Err(astrcode_core::extension::ExtensionError::NotFound(
                tool_name.into(),
            ))
        }
    }

    #[tokio::test]
    async fn child_extra_system_prompt_participates_in_snapshot_build() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        let prompt_files = load_system_prompt_files(".").await;
        let (system_prompt, fingerprint) =
            build_system_prompt_snapshot_with_files(SystemPromptSnapshotInput {
                extension_runner: &runner,
                session_id: "session-1",
                working_dir: ".",
                model_id: "mock",
                tools: &[],
                extra_system_prompt: Some("child body"),
                tool_prompt_metadata: HashMap::new(),
                prompt_files,
            })
            .await
            .unwrap();
        assert!(system_prompt.contains("child body"));
        assert!(!fingerprint.is_empty());
    }

    #[tokio::test]
    async fn tool_snapshot_precedence_is_explicit() {
        let runner = ExtensionRunner::new(Duration::from_secs(1));
        runner
            .register(Arc::new(StaticToolExtension {
                id: "first",
                tool_name: "shell",
                description: "first extension shell",
            }))
            .await;
        runner
            .register(Arc::new(StaticToolExtension {
                id: "second",
                tool_name: "shell",
                description: "second extension shell",
            }))
            .await;
        let registry = build_tool_registry_snapshot(&runner, ".", 1).await;
        let shell = registry.find_definition("shell").unwrap();
        assert_eq!(shell.origin, ToolOrigin::Extension);
        assert_eq!(shell.description, "first extension shell");
    }
}
